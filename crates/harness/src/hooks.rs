//! Hooks — 生命周期钩子：在工具调用前后 / turn 结束 / 会话开始时执行用户命令。
//!
//! 协议参考 Claude Code hooks（dsh hooks-claude-code 同款语义）：
//! - payload JSON 写 stdin 并随即关闭；进程 stdio 全部接管（Windows 下不闪窗口）。
//! - 退出码 2 = 阻断，stderr（截断 500 字符）为原因；其他非零退出码只记 warn、不阻断。
//! - exit 0 时 stdout 若是 JSON 对象且 `decision=="deny"`（附 `reason`）同样阻断；
//!   `decision=="allow"` 放行；非 JSON stdout 按普通输出忽略。
//! - 超时（timeout_ms）杀进程并忽略该 hook——超时不阻断，防止 hook 卡死 agent；
//!   超时前已退出的 exit-2 仍然生效。
//! - matcher 是工具名 glob（`*`/`?`/`**`，空或 `*` 匹配全部），只作用于
//!   Pre/PostToolUse；同一事件多个 hook 按配置顺序执行，阻断即短路。

use crate::error::HarnessError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// 阻断退出码（Claude Code hooks 方言）。
const BLOCK_EXIT_CODE: i32 = 2;
/// 阻断原因（stderr / deny reason）的最大字符数。
const MAX_REASON_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    PreToolUse,
    PostToolUse,
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    pub event: HookEvent,
    /// 工具名 glob（`*` = 全部；仅 Pre/PostToolUse 用）。
    #[serde(default = "star")]
    pub matcher: String,
    pub command: String,
    /// 超时毫秒（缺省 10s；超时 = 忽略该 hook，不阻断）。
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn star() -> String {
    "*".into()
}
fn default_timeout() -> u64 {
    10_000
}

/// hooks 是否为空（调用方快速跳过）。
pub fn is_empty(hooks: &[HookConfig], event: HookEvent) -> bool {
    !hooks.iter().any(|h| h.event == event)
}

/// PreToolUse：返回 Err(HarnessError::Tool(reason)) = 阻断这次工具调用。
pub async fn run_pre_tool(
    hooks: &[HookConfig],
    tool: &str,
    args: &Value,
    payload: Value,
) -> Result<(), HarnessError> {
    fire(hooks, HookEvent::PreToolUse, Some(tool), Some(args), payload).await
}

/// PostToolUse：通知型，失败只记日志。
pub async fn run_post_tool(hooks: &[HookConfig], tool: &str, payload: Value) {
    let _ = fire(hooks, HookEvent::PostToolUse, Some(tool), None, payload).await;
}

/// Stop：turn 收尾。返回 Err = 建议不要停止（原因给用户）。
pub async fn run_stop(hooks: &[HookConfig], payload: Value) -> Result<(), HarnessError> {
    fire(hooks, HookEvent::Stop, None, None, payload).await
}

/// SessionStart：通知型。
pub async fn run_session_start(hooks: &[HookConfig], payload: Value) {
    let _ = fire(hooks, HookEvent::SessionStart, None, None, payload).await;
}

// ------------------------------------------------------------------ 内部实现

/// 单次 hook 执行的原始结果。`exit_code = None` 表示没有干净的退出码
/// （spawn 失败 / 等待出错 / 被信号杀死），一律按非阻断处理。
#[derive(Debug, Default)]
struct HookOutcome {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    /// 超时被杀：协议规定一律忽略，不阻断。
    timed_out: bool,
    /// spawn / 等待阶段的错误描述（非阻断）。
    error: Option<String>,
}

/// 驱动一个事件的全部 hook：按配置顺序执行，阻断即短路返回 Err。
async fn fire(
    hooks: &[HookConfig],
    event: HookEvent,
    tool: Option<&str>,
    args: Option<&Value>,
    payload: Value,
) -> Result<(), HarnessError> {
    let stdin_json = merge_payload(payload, tool, args);
    for hook in hooks.iter().filter(|h| h.event == event) {
        // matcher 只对工具类事件有意义；Stop/SessionStart 的 hook 全部执行。
        if let Some(name) = tool {
            if !matcher_matches(&hook.matcher, name) {
                continue;
            }
        }
        let started = Instant::now();
        // 每个 hook 独立 spawn：调用方中途取消时，任务仍在自己的超时保护下
        // 收敛（到点杀进程），不会留下失控子进程，也不会拖住调用方。
        let task = tokio::spawn(run_one_hook(hook.clone(), stdin_json.clone()));
        let outcome = match task.await {
            Ok(outcome) => outcome,
            Err(e) => HookOutcome {
                error: Some(format!("hook 任务失败: {e}")),
                ..Default::default()
            },
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;

        if let Some(reason) = blocking_reason(&outcome) {
            tracing::debug!(event = ?event, command = %hook.command, elapsed_ms,
                result = "block", reason = %reason, "hook 阻断");
            return Err(HarnessError::Tool(reason));
        }
        log_outcome(event, &hook, elapsed_ms, &outcome);
    }
    Ok(())
}

/// 单个 hook 的完整执行：起进程 → stdin 写 payload → 限时收 stdout/stderr。
async fn run_one_hook(hook: HookConfig, stdin_json: String) -> HookOutcome {
    let mut cmd = make_command(&hook.command);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // 与 tools/shell.rs 同款：别在 GUI 场景给每个 hook 弹控制台窗口。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return HookOutcome {
                error: Some(format!("spawn 失败: {e}")),
                ..Default::default()
            };
        }
    };

    // 并发写 stdin 并随即关闭：payload 可能超过管道缓冲，孩子不读时也不能
    // 卡住主等待；孩子提前退出导致 EPIPE 属正常，写失败静默即可。
    // writer 独立 detach：孩子退出（wait_with_output 返回）后它必然收敛，
    // 唯一例外是 hook 留下持管道的后台孙进程——不能为它挂住主流程。
    let mut stdin = child.stdin.take().expect("stdin 已设为 piped");
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(stdin_json.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    drop(writer); // detach

    // 并发收 stdout/stderr 再等退出（绝不串行地“先等退出再读流”——那是
    // 经典死锁）。timeout 到点：内层 future 被 drop → kill_on_drop 杀掉进程，
    // 协议规定超时 = 忽略该 hook（不阻断）；hook 若留下持管道的后台孙进程，
    // 最坏也退化成超时忽略——这正是超时的安全阀语义。
    let waited =
        tokio::time::timeout(Duration::from_millis(hook.timeout_ms), child.wait_with_output()).await;
    match waited {
        Err(_) => HookOutcome {
            timed_out: true,
            ..Default::default()
        },
        Ok(Err(e)) => HookOutcome {
            error: Some(format!("等待进程失败: {e}")),
            ..Default::default()
        },
        Ok(Ok(output)) => HookOutcome {
            exit_code: output.status.code(),
            stdout: decode_bytes(&output.stdout),
            stderr: decode_bytes(&output.stderr),
            ..Default::default()
        },
    }
}

/// 把 hook 命令包进 shell。Windows 用 `cmd /C <command>`，且必须用 `raw_arg`
/// 原样传参：std/tokio 默认按 MSVCRT 规则给含空格/引号的参数做 `\"` 转义，
/// cmd.exe 不认这套转义，会把带引号的命令行弄坏；raw_arg 逐字节透传，
/// 整条命令作为一个字符串交给 cmd /C。Unix 用 `sh -c <command>`。
fn make_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.raw_arg("/C");
        cmd.raw_arg(command);
        return cmd;
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c");
        cmd.arg(command);
        return cmd;
    }
}

/// matcher 匹配工具名：空串或 `*` 匹配全部；否则按 globset glob
/// （`*`/`?`/`**`）匹配。pattern 非法时退化为字面量相等（与 permission.rs 一致）。
fn matcher_matches(matcher: &str, tool: &str) -> bool {
    let pattern = matcher.trim();
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    use globset::Glob;
    match Glob::new(pattern) {
        Ok(glob) => glob.compile_matcher().is_match(tool),
        Err(_) => pattern == tool,
    }
}

/// 按协议解读一次执行结果；`Some(reason)` = 阻断。
/// - 退出码 2：阻断，stderr（截断 500 字符）为原因，stderr 为空时给兜底文案；
/// - 退出码 0 且 stdout 以 `{` 开头：解析 JSON，`decision=="deny"` 阻断
///   （reason 缺省给兜底），`decision=="allow"` 明确放行，畸形 JSON 宽松忽略；
/// - 其他一切（非零非 2、无退出码、超时、spawn 失败）：不阻断。
fn blocking_reason(outcome: &HookOutcome) -> Option<String> {
    if outcome.exit_code == Some(BLOCK_EXIT_CODE) {
        let stderr = outcome.stderr.trim();
        let reason = if stderr.is_empty() {
            "hook blocked (exit code 2)".to_owned()
        } else {
            truncate_chars(stderr, MAX_REASON_CHARS)
        };
        return Some(reason);
    }
    if outcome.exit_code != Some(0) || !outcome.stdout.trim().starts_with('{') {
        return None;
    }
    let parsed: Value = match serde_json::from_str(outcome.stdout.trim()) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let map = match parsed.as_object() {
        Some(m) => m,
        None => return None,
    };
    match map.get("decision").and_then(Value::as_str) {
        Some("deny") => {
            let reason = map
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("denied by hook");
            Some(truncate_chars(reason, MAX_REASON_CHARS))
        }
        _ => None, // allow / 其他 decision：放行
    }
}

/// 组装写入 hook stdin 的 JSON：调用方构造的 payload 为准（已含全部字段），
/// 仅在对象缺 `tool`/`args` 键时用参数兜底补上。非对象 payload 原样透传。
fn merge_payload(payload: Value, tool: Option<&str>, args: Option<&Value>) -> String {
    let mut payload = if payload.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        payload
    };
    if let Value::Object(map) = &mut payload {
        if let Some(name) = tool {
            map.entry("tool".to_owned())
                .or_insert_with(|| Value::String(name.to_owned()));
        }
        if let Some(a) = args {
            map.entry("args".to_owned()).or_insert_with(|| a.clone());
        }
    }
    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned())
}

/// 每个 hook 一条执行日志；异常（超时 / 失败 / 非零退出）升到 warn。
fn log_outcome(event: HookEvent, hook: &HookConfig, elapsed_ms: u64, outcome: &HookOutcome) {
    if outcome.timed_out {
        tracing::warn!(event = ?event, command = %hook.command,
            timeout_ms = hook.timeout_ms, elapsed_ms,
            "hook 超时被杀，忽略（不阻断）");
    } else if let Some(err) = &outcome.error {
        tracing::warn!(event = ?event, command = %hook.command, elapsed_ms,
            error = %err, "hook 执行失败，忽略（不阻断）");
    } else if let Some(code) = outcome.exit_code.filter(|c| *c != 0) {
        tracing::warn!(event = ?event, command = %hook.command, elapsed_ms,
            exit_code = code, "hook 非零退出，忽略（不阻断）");
    } else {
        tracing::debug!(event = ?event, command = %hook.command, elapsed_ms,
            result = "pass", "hook 放行/完成");
    }
}

/// 截断到最多 `max` 个字符（按字符计，不是字节）。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect()
    }
}

/// stdout/stderr 解码：严格 UTF-8 优先；不是合法 UTF-8 时按 GB18030 解
/// （Windows cmd 的系统 ANSI 码页，中文是 GBK/GB18030），与 tools/shell.rs 同策略。
fn decode_bytes(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_owned(),
        Err(_) => {
            let (cow, _, _) = encoding_rs::GB18030.decode(raw);
            cow.into_owned()
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    //! 用例全部依赖真实进程（cmd.exe 的退出码 / echo、PowerShell 的 stdin 落盘），
    //! 只在 Windows 上编译运行。
    use super::*;
    use serde_json::json;

    fn hook(event: HookEvent, matcher: &str, command: &str, timeout_ms: u64) -> HookConfig {
        HookConfig {
            event,
            matcher: matcher.to_owned(),
            command: command.to_owned(),
            timeout_ms,
        }
    }

    /// 用 PowerShell 把 hook 收到的 stdin 原样落盘（WriteAllText 默认 UTF-8 无 BOM）。
    fn capture_command(path: &std::path::Path) -> String {
        format!(
            "powershell -NoProfile -Command \"[IO.File]::WriteAllText('{}', [Console]::In.ReadToEnd())\"",
            path.to_string_lossy()
        )
    }

    // ---------- 纯逻辑：matcher / 截断 / 决策矩阵 ----------

    #[test]
    fn matcher_glob_semantics() {
        assert!(matcher_matches("", "Bash")); // 空 = 全部
        assert!(matcher_matches("*", "Bash"));
        assert!(matcher_matches("Bash", "Bash"));
        assert!(matcher_matches("Ba*", "Bash"));
        assert!(matcher_matches("Ba??", "Bash"));
        assert!(matcher_matches("**", "mcp__fs__read"));
        assert!(matcher_matches("mcp__*", "mcp__fs__read"));
        assert!(!matcher_matches("Ba?", "Bash"));
        assert!(!matcher_matches("Read", "Bash"));
        assert!(!matcher_matches("Read*", "Edit"));
        // 非法 pattern 退化为字面量相等
        assert!(!matcher_matches("Ba[", "Bash"));
        assert!(matcher_matches("Ba[", "Ba["));
    }

    #[test]
    fn stderr_reason_truncated_to_500() {
        let outcome = HookOutcome {
            exit_code: Some(2),
            stderr: "x".repeat(2000),
            ..Default::default()
        };
        let reason = blocking_reason(&outcome).expect("exit 2 blocks");
        assert_eq!(reason.chars().count(), MAX_REASON_CHARS);
        assert!(reason.starts_with("xxx"));

        // deny JSON 的 reason 同样截断
        let outcome = HookOutcome {
            exit_code: Some(0),
            stdout: format!("{{\"decision\":\"deny\",\"reason\":\"{}\"}}", "y".repeat(2000)),
            ..Default::default()
        };
        let reason = blocking_reason(&outcome).expect("deny blocks");
        assert_eq!(reason.chars().count(), MAX_REASON_CHARS);
    }

    #[test]
    fn blocking_decision_matrix() {
        // exit 0 + 普通 stdout → 放行
        let pass = HookOutcome { exit_code: Some(0), stdout: "hello\n".into(), ..Default::default() };
        assert!(blocking_reason(&pass).is_none());
        // exit 0 + allow → 放行
        let allow = HookOutcome {
            exit_code: Some(0),
            stdout: "{\"decision\":\"allow\"}".into(),
            ..Default::default()
        };
        assert!(blocking_reason(&allow).is_none());
        // exit 0 + deny → 阻断，reason 取自 JSON
        let deny = HookOutcome {
            exit_code: Some(0),
            stdout: "{\"decision\":\"deny\",\"reason\":\"no way\"}".into(),
            ..Default::default()
        };
        assert_eq!(blocking_reason(&deny).as_deref(), Some("no way"));
        // deny 无 reason → 兜底文案，仍阻断
        let deny_no_reason = HookOutcome {
            exit_code: Some(0),
            stdout: "{\"decision\":\"deny\"}".into(),
            ..Default::default()
        };
        assert!(blocking_reason(&deny_no_reason).is_some());
        // 畸形 JSON → 宽松放行
        let malformed = HookOutcome {
            exit_code: Some(0),
            stdout: "{\"decision\":\"deny\"".into(),
            ..Default::default()
        };
        assert!(blocking_reason(&malformed).is_none());
        // 非零非 2 → 忽略（结构化 stdout 也不生效）
        let exit1 = HookOutcome {
            exit_code: Some(1),
            stdout: "{\"decision\":\"deny\"}".into(),
            ..Default::default()
        };
        assert!(blocking_reason(&exit1).is_none());
        // 无退出码（spawn 失败 / 等待出错）→ 忽略
        assert!(blocking_reason(&HookOutcome::default()).is_none());
        // 超时被杀 → 忽略
        let timed_out = HookOutcome { timed_out: true, ..Default::default() };
        assert!(blocking_reason(&timed_out).is_none());
    }

    // ---------- 真实进程：退出码语义 ----------

    #[tokio::test]
    async fn exit_zero_allows() {
        let hooks = [hook(HookEvent::PreToolUse, "*", "exit 0", 10_000)];
        let payload = json!({"event":"pre_tool_use","tool":"Bash","args":{},
            "session":"s","workspace":"w","timestamp":1u64});
        assert!(run_pre_tool(&hooks, "Bash", &json!({}), payload).await.is_ok());
    }

    #[tokio::test]
    async fn exit_two_blocks() {
        let hooks = [hook(HookEvent::PreToolUse, "*", "exit 2", 10_000)];
        let err = run_pre_tool(&hooks, "Bash", &json!({}), json!({"event":"pre_tool_use"}))
            .await
            .expect_err("exit 2 must block");
        assert!(
            matches!(err, HarnessError::Tool(ref m) if m.contains("exit code 2")),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn exit_two_stderr_becomes_reason() {
        let hooks = [hook(HookEvent::PreToolUse, "*", "echo oops 1>&2 & exit 2", 10_000)];
        let err = run_pre_tool(&hooks, "Bash", &json!({}), json!({}))
            .await
            .expect_err("exit 2 must block");
        assert!(
            matches!(err, HarnessError::Tool(ref m) if m.contains("oops")),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn other_nonzero_exit_ignored() {
        let hooks = [hook(HookEvent::PreToolUse, "*", "exit 1", 10_000)];
        assert!(run_pre_tool(&hooks, "Bash", &json!({}), json!({})).await.is_ok());
    }

    #[tokio::test]
    async fn deny_json_blocks_allow_and_plain_stdout_pass() {
        let deny = [hook(
            HookEvent::PreToolUse,
            "*",
            "echo {\"decision\":\"deny\",\"reason\":\"no way\"}",
            10_000,
        )];
        let err = run_pre_tool(&deny, "Bash", &json!({}), json!({}))
            .await
            .expect_err("deny must block");
        assert!(
            matches!(err, HarnessError::Tool(ref m) if m.contains("no way")),
            "got: {err}"
        );

        let allow = [hook(HookEvent::PreToolUse, "*", "echo {\"decision\":\"allow\"}", 10_000)];
        assert!(run_pre_tool(&allow, "Bash", &json!({}), json!({})).await.is_ok());

        let plain = [hook(HookEvent::PreToolUse, "*", "echo hello", 10_000)];
        assert!(run_pre_tool(&plain, "Bash", &json!({}), json!({})).await.is_ok());
    }

    #[tokio::test]
    async fn timeout_ignored_and_killed_promptly() {
        let started = Instant::now();
        // ping -n 30 ≈ 挂 29 秒；超时 400ms 必须杀掉并放行，不能陪它等完。
        let hooks = [hook(HookEvent::PreToolUse, "*", "ping -n 30 127.0.0.1 > NUL", 400)];
        assert!(run_pre_tool(&hooks, "Bash", &json!({}), json!({})).await.is_ok());
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "timeout must not wait for the child"
        );
    }

    // ---------- matcher 过滤（真实进程） ----------

    #[tokio::test]
    async fn matcher_filters_real_hooks() {
        // matcher 不匹配 → hook 不执行 → 不阻断
        let hooks = [hook(HookEvent::PreToolUse, "Write*", "exit 2", 10_000)];
        assert!(run_pre_tool(&hooks, "Read", &json!({}), json!({})).await.is_ok());
        // `?` 通配命中 → 阻断
        let hooks = [hook(HookEvent::PreToolUse, "Re?d", "exit 2", 10_000)];
        assert!(run_pre_tool(&hooks, "Read", &json!({}), json!({})).await.is_err());
        // 空 matcher = 全部
        let hooks = [hook(HookEvent::PreToolUse, "", "exit 2", 10_000)];
        assert!(run_pre_tool(&hooks, "Anything", &json!({}), json!({})).await.is_err());
    }

    // ---------- payload 形状 ----------

    #[tokio::test]
    async fn payload_json_reaches_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("stdin.json");
        let command = capture_command(&out);
        let hooks = [hook(HookEvent::PreToolUse, "*", &command, 60_000)];
        let payload = json!({
            "event": "pre_tool_use",
            "tool": "Bash",
            "args": {"command": "ls -la"},
            "session": "sess-1",
            "workspace": "C:\\tmp",
            "timestamp": 1234u64,
        });
        run_pre_tool(&hooks, "Bash", &json!({"command":"ls -la"}), payload.clone())
            .await
            .expect("exit-0 hook passes");
        let received: Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(received, payload, "hook 必须原样收到完整 payload");
    }

    #[tokio::test]
    async fn payload_merge_fills_tool_and_args() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("stdin.json");
        let command = capture_command(&out);
        let hooks = [hook(HookEvent::PreToolUse, "*", &command, 60_000)];
        // payload 没带 tool/args：实现里用参数兜底补齐
        run_pre_tool(&hooks, "Bash", &json!({"command":"ls"}), json!({"event":"pre_tool_use"}))
            .await
            .expect("exit-0 hook passes");
        let received: Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(
            received,
            json!({"event":"pre_tool_use","tool":"Bash","args":{"command":"ls"}})
        );
    }

    // ---------- 事件路由 / 短路 / 通知型事件 ----------

    #[tokio::test]
    async fn other_event_hooks_do_not_fire() {
        // Stop 的 hook 不应拦 PreToolUse
        let hooks = [hook(HookEvent::Stop, "*", "exit 2", 10_000)];
        assert!(run_pre_tool(&hooks, "Bash", &json!({}), json!({})).await.is_ok());
    }

    #[tokio::test]
    async fn first_blocking_hook_short_circuits() {
        let hooks = [
            hook(
                HookEvent::PreToolUse,
                "*",
                "echo {\"decision\":\"deny\",\"reason\":\"first\"}",
                10_000,
            ),
            hook(HookEvent::PreToolUse, "*", "echo oops-second 1>&2 & exit 2", 10_000),
        ];
        let err = run_pre_tool(&hooks, "Bash", &json!({}), json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, HarnessError::Tool(ref m) if m.contains("first")),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn stop_blocks_and_notify_events_never_fail() {
        // Stop：exit 2 = 建议不要停止
        let hooks = [hook(HookEvent::Stop, "*", "exit 2", 10_000)];
        assert!(run_stop(&hooks, json!({"event":"stop","session":"s"})).await.is_err());
        // SessionStart / PostToolUse：通知型，即使 exit 2 也只记日志、不炸调用方
        let hooks = [
            hook(HookEvent::SessionStart, "*", "exit 2", 10_000),
            hook(HookEvent::PostToolUse, "*", "exit 2", 10_000),
        ];
        run_session_start(&hooks, json!({"event":"session_start","session":"s","workspace":"w"})).await;
        run_post_tool(&hooks, "Bash", json!({"event":"post_tool_use","tool":"Bash"})).await;
    }

    #[test]
    fn is_empty_filters_by_event() {
        let hooks = [hook(HookEvent::PreToolUse, "*", "exit 0", 10_000)];
        assert!(!is_empty(&hooks, HookEvent::PreToolUse));
        assert!(is_empty(&hooks, HookEvent::Stop));
    }
}
