//! `bash` tool: run a command, stream its output, enforce a timeout.

use super::*;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};

const MAX_OUTPUT: usize = 24_000;

fn shell_pair() -> (String, Vec<String>) {
    // Git Bash / MSYS on Windows keeps `grep`, `ls`, `cargo` etc. usable; cmd.exe
    // does not. Prefer bash when it is on the PATH or in the usual install spots.
    if cfg!(windows) {
        for candidate in [
            "C:/Program Files/Git/bin/bash.exe",
            "C:/Program Files (x86)/Git/bin/bash.exe",
        ] {
            if std::path::Path::new(candidate).exists() {
                return (candidate.to_string(), vec!["-lc".into()]);
            }
        }
        let from_path = std::process::Command::new("where")
            .arg("bash")
            .output()
            .ok()
            .and_then(|o| {
                decode_bytes(&o.stdout)
                    .lines()
                    .next()
                    .map(|s| s.trim().to_string())
            })
            .filter(|s| !s.is_empty());
        if let Some(b) = from_path {
            return (b, vec!["-lc".into()]);
        }
        return ("cmd".into(), vec!["/C".into()]);
    }
    ("bash".into(), vec!["-lc".into()])
}

/// 终止整个进程树。Windows 用 `taskkill /F /T`(递归杀掉孙进程);Unix 上
/// 孩子在独立进程组里(spawn 前设置了 process_group(0)),对负 PID 发
/// SIGKILL 整组清掉。外部工具失败时回退 start_kill,至少杀掉直接孩子。
fn kill_tree_pid(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID"])
            .arg(pid.to_string())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    #[cfg(unix)]
    {
        // 负 PID 表示对整个进程组发信号
        let _ = std::process::Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .status();
    }
}

fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        kill_tree_pid(pid);
    }
    let _ = child.start_kill();
}

/// Drop 时收拾整棵进程树。
///
/// `kill_on_drop` 只杀直接孩子；Git Bash 跑 `sleep` 时会留下孙进程继续跑。
/// 终止（future 被 drop）时必须由 guard 递归清理，否则 UI 显示“已终止”但
/// 命令仍在后台改文件。正常退出/超时路径显式 disarm，避免误杀被复用的 PID。
struct ProcessTreeGuard {
    pid: u32,
    armed: bool,
}

impl ProcessTreeGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid: pid.unwrap_or(0), armed: pid.is_some() }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        if self.armed && self.pid != 0 {
            kill_tree_pid(self.pid);
        }
    }
}

/// 把一行原始字节转成字符串,去掉行尾换行。
///
/// Windows 上 cmd / PowerShell / 多数原生程序输出的是**系统 ANSI 码页**
/// (简体中文是 GBK/GB18030),按 UTF-8 解会整段变成 `�`。所以先严格试
/// UTF-8,不是合法 UTF-8 时回退到 GB18030(GBK 的超集,兼容 CP936)。
/// 两者都不成立才用替换符兜底,不让整段输出丢失。
fn decode_line(raw: &[u8]) -> String {
    let s = decode_bytes(raw);
    s.trim_end_matches(|c| c == '\r' || c == '\n')
        .to_string()
}

/// 按内容猜编码地解码一段字节。
fn decode_bytes(raw: &[u8]) -> String {
    // 严格 UTF-8 优先:Unix 工具与刻意输出 UTF-8 的程序都走这条。
    if let Ok(s) = std::str::from_utf8(raw) {
        return s.to_string();
    }
    // Windows 系统码页:简体中文是 CP936(GBK),GB18030 是其超集。
    let (cow, _, _) = encoding_rs::GB18030.decode(raw);
    cow.into_owned()
}

/// Run `command` in `cwd`, streaming its output.
///
/// The three stream parameters are what the shell needs to publish live
/// output: the event hub to publish to, the session the output belongs to
/// (empty = broadcast), and the call id the client filters progress by.
/// Keeping them explicit lets callers outside the tool layer — the side
/// pane's terminal panel — run a command without fabricating a `ToolCtx`.
pub async fn bash(
    command: &str,
    cwd: &Path,
    timeout_ms: u64,
    hub: &crate::event::EventHub,
    sid: &str,
    cid: &str,
) -> Result<ToolOutcome, HarnessError> {
    let (prog, args) = shell_pair();
    let mut cmd = tokio::process::Command::new(&prog);
    cmd.args(&args)
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 后台终端靠 abort 任务来“杀掉”命令：没有这个开关，JoinHandle 被
        // abort 后子进程会继续跑（还在往管道里写），用户看到“已终止”但进程
        // 仍在。drop 即杀保证 UI 与真实状态一致。
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    // Unix 上把孩子放进独立进程组:超时才能对整组发信号,孙进程也跑不掉
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| HarnessError::Tool(format!("failed to spawn {prog}: {e}")))?;
    // 必须先于 child 声明：Rust 逆序 drop，取消时 guard 先 taskkill /T，
    // 再让 child 的 kill_on_drop 收尾，孙进程不会残留。
    let mut tree_guard = ProcessTreeGuard::new(child.id());

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let hub = hub.clone();
    let sid = sid.to_string();
    let cid = cid.to_string();

    // 输出缓冲放在共享 Mutex 里:超时/卡死路径不等 EOF 也能把已收到的部分
    // 带走,不会因为管道被残留孙进程攥着而丢内容。
    let out_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let err_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let out_task = tokio::spawn({
        let buf = out_buf.clone();
        async move {
            if let Some(out) = stdout {
                let mut reader = BufReader::new(out);
                let mut raw = Vec::new();
                loop {
                    raw.clear();
                    // 按字节读行再 lossy 解码:坏 UTF-8 不再让整段输出丢失
                    match reader.read_until(b'\n', &mut raw).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let line = decode_line(&raw);
                            // 缓冲满后既不再累积也不再发 progress,防刷屏
                            let emit = {
                                let mut g = buf.lock().unwrap();
                                let emit = g.len() < MAX_OUTPUT;
                                if emit {
                                    g.push_str(&line);
                                    g.push('\n');
                                }
                                emit
                            };
                            if emit {
                                hub.publish(&sid, "tool.progress", json!({"callId": cid, "chunk": line}));
                            }
                        }
                    }
                }
            }
        }
    });
    let err_task = tokio::spawn({
        let buf = err_buf.clone();
        async move {
            if let Some(err) = stderr {
                let mut reader = BufReader::new(err);
                let mut raw = Vec::new();
                loop {
                    raw.clear();
                    match reader.read_until(b'\n', &mut raw).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let line = decode_line(&raw);
                            let mut g = buf.lock().unwrap();
                            if g.len() < MAX_OUTPUT {
                                g.push_str(&line);
                                g.push('\n');
                            }
                        }
                    }
                }
            }
        }
    });

    let status = tokio::select! {
        st = child.wait() => st.map_err(|e| HarnessError::Tool(format!("wait failed: {e}")))?,
        _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms.max(1000))) => {
            // 超时:整树击杀,再有限收尾
            kill_tree(&mut child);
            let _ = child.wait().await;
            tree_guard.disarm();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                out_task.await.ok();
                err_task.await.ok();
            })
            .await;
            let stdout = out_buf.lock().unwrap().clone();
            let stderr = err_buf.lock().unwrap().clone();
            let stderr_msg = if stderr.trim().is_empty() {
                "process was killed after the timeout".to_string()
            } else {
                stderr
            };
            return Ok(ToolOutcome {
                ok: false,
                title: format!("Timed out after {}ms", timeout_ms),
                display: DisplayKind::Shell {
                    command: command.to_string(),
                    stdout: clamp(&stdout, MAX_OUTPUT),
                    stderr: stderr_msg,
                    exit: -1,
                },
                for_model: format!(
                    "Command timed out after {timeout_ms}ms and was killed:\n{command}\nPartial output:\n{}",
                    clamp(&stdout, MAX_OUTPUT)
                ),
            });
        }
    };

    // 孩子已正常退出，树 guard 不再需要；提前 disarm，避免后续输出收尾
    // 期间误杀同 PID 的新进程。
    tree_guard.disarm();

    // 孩子已退出;管道 EOF 偶尔会被残留孙进程攥住,最多再等 3 秒,
    // 超时就以已有缓冲收尾,不让整条命令挂死。
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        out_task.await.ok();
        err_task.await.ok();
    })
    .await;
    let stdout = out_buf.lock().unwrap().clone();
    let stderr = err_buf.lock().unwrap().clone();
    let code = status.code().unwrap_or(-1);

    let title = if code == 0 {
        format!("Ran `{}` · exit 0", truncate(command, 48))
    } else {
        format!("Ran `{}` · exit {code}", truncate(command, 48))
    };

    let mut for_model = String::new();
    for_model.push_str(&format!("exit code: {code}\n"));
    if !stdout.trim().is_empty() {
        for_model.push_str("stdout:\n");
        for_model.push_str(&clamp(&stdout, MAX_OUTPUT));
        for_model.push('\n');
    }
    if !stderr.trim().is_empty() {
        for_model.push_str("stderr:\n");
        for_model.push_str(&clamp(&stderr, MAX_OUTPUT));
    }
    if stdout.trim().is_empty() && stderr.trim().is_empty() {
        for_model.push_str("(no output)");
    }

    Ok(ToolOutcome {
        ok: code == 0,
        title,
        display: DisplayKind::Shell {
            command: command.to_string(),
            stdout: clamp(&stdout, MAX_OUTPUT),
            stderr: clamp(&stderr, MAX_OUTPUT),
            exit: code,
        },
        for_model,
    })
}

/// 超长输出保留头部一小段 + 尾部大段(命令结果的关键通常在尾部),中间
/// 插入省略提示。切点都回退到字符边界,避免按字节切片 panic。
fn clamp(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    const HEAD: usize = 1000;
    let head_end = floor_boundary(s, HEAD.min(max));
    let tail_len = max.saturating_sub(HEAD);
    let tail_start = floor_boundary(s, s.len().saturating_sub(tail_len));
    if tail_len == 0 || tail_start <= head_end {
        // 上限太小放不下头尾两段,退化为只留头部
        return format!(
            "{}\n… [truncated {} bytes total] …",
            &s[..head_end],
            s.len()
        );
    }
    format!(
        "{}\n… [truncated {} bytes] …\n{}",
        &s[..head_end],
        tail_start - head_end,
        &s[tail_start..]
    )
}

/// 向后回退到最近的字符边界。
fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_head_and_tail() {
        let s = format!("{}{}{}", "A".repeat(1000), "B".repeat(30_000), "C".repeat(500));
        let out = clamp(&s, 4000);
        assert!(out.starts_with("AAAA"), "must keep head, got {out:?}");
        assert!(out.ends_with("CCCC"), "must keep tail, got {out:?}");
        assert!(out.contains("… [truncated "), "must mark the cut");
        assert!(out.len() < 6000, "bounded output, got {}", out.len());
    }

    #[test]
    fn clamp_is_boundary_safe() {
        // 每个字 3 字节,1000 与 19000 的切点都落在字符中间,不能 panic
        let s = "中".repeat(8000);
        let out = clamp(&s, 20_000);
        assert!(out.contains('中'));
        assert!(out.contains("… [truncated "));
    }

    #[test]
    fn clamp_small_limit_head_only() {
        let s = "x".repeat(100);
        let out = clamp(&s, 10);
        assert!(out.starts_with("xxx"));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn clamp_short_passes_through() {
        assert_eq!(clamp("abc", 100), "abc");
    }

    #[test]
    fn decode_line_lossy_and_trims_eol() {
        assert_eq!(decode_line(b"hello\r\n"), "hello");
        assert_eq!(decode_line(b"hello\n"), "hello");
        assert_eq!(decode_line(b"hello"), "hello");
    }

    #[test]
    fn decode_line_reads_gbk_chinese() {
        // Windows 命令的输出是系统 ANSI 码页(中文 CP936/GBK)。
        // 按 UTF-8 解会整段变乱码 —— 这正是「命令跑出来全是 �」的根因。
        let gbk: Vec<u8> = encoding_rs::GBK.encode("无法识别 MAYBE_FIRST_START.Name 的名称").0.into_owned();
        assert!(!gbk.is_ascii());
        // 不是合法 UTF-8，必须走 GB18030 回退
        assert!(std::str::from_utf8(&gbk).is_err());
        assert_eq!(decode_line(&gbk), "无法识别 MAYBE_FIRST_START.Name 的名称");
    }

    #[test]
    fn decode_line_prefers_utf8() {
        // 合法 UTF-8 优先，不能被 GBK 误解码
        assert_eq!(decode_line("中文 ok".as_bytes()), "中文 ok");
    }

    #[test]
    fn decode_line_survives_undecodable_bytes() {
        // 两边都解不了时用替换符兜底，行不能丢
        assert_eq!(decode_line(&[0x61, 0xff, 0x62]), "a\u{FFFD}b");
    }
}

#[cfg(test)]
mod live_windows_tests {
    //! 在真实 Windows 上验证码页解码（只在 Windows 上跑）。
    use super::*;

    #[tokio::test]
    #[cfg(windows)]
    async fn decodes_real_gbk_command_output() {
        // PowerShell 的中文错误信息是 GBK/CP936 —— 之前整段显示为乱码。
        // 用 cmd 触发一条中文错误消息（"不是内部或外部命令"），它是 GBK 编码。
        let out = tokio::process::Command::new("cmd")
            .args(["/C", "echo 中文测试 & 不存在的命令名"])
            .output()
            .await;
        let out = match out {
            Ok(o) => o,
            Err(_) => return,   // 环境没有 powershell，跳过
        };
        let raw: Vec<u8> = [
            out.stdout.as_slice(),
            out.stderr.as_slice(),
        ].concat();
        if raw.is_empty() {
            return;
        }
        let decoded = decode_bytes(&raw);
        // 关键：不能出现替换符（那说明按错误编码解了）
        assert!(
            !decoded.contains('\u{FFFD}'),
            "decoded output must not contain replacement chars: {decoded}"
        );
        // 至少要能解出中文
        let has_cjk = decoded.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
        assert!(has_cjk, "expected CJK text in decoded output, got: {decoded}");
    }
}
