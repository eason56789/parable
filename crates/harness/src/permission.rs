//! Permission gateway: risk classification, allow/deny rule table, and the broker
//! that suspends a tool call until the user decides.

use crate::error::HarnessError;
use crate::event::EventHub;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    /// Read-only, safe in any mode.
    ReadOnly,
    /// Modifies files inside the workspace.
    Write,
    /// Deletes or overwrites in a way that is hard to undo.
    Destructive,
    /// Talks to the network.
    Network,
    /// Executes a shell command.
    Shell,
}

impl Risk {
    pub fn as_str(&self) -> &'static str {
        match self {
            Risk::ReadOnly => "readonly",
            Risk::Write => "write",
            Risk::Destructive => "destructive",
            Risk::Network => "network",
            Risk::Shell => "shell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Once,
    Always,
    Deny,
}

impl Decision {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "once" | "ok" | "allow" => Some(Decision::Once),
            "always" => Some(Decision::Always),
            "no" | "deny" => Some(Decision::Deny),
            _ => None,
        }
    }
    pub fn allowed(&self) -> bool {
        matches!(self, Decision::Once | Decision::Always)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// Tool name, or `*` for any tool.
    #[serde(default = "star")]
    pub tool: String,
    /// Glob-ish path pattern; `*` matches anything. `exact` 规则里存的是字面量
    /// (shell 类工具存的是命令原文)。
    #[serde(default = "star")]
    pub path: String,
    /// `allow` | `deny`
    pub action: String,
    /// true = 精确等值匹配(Always 决策持久化出来的规则);false/缺省 = glob
    /// 语义(permissions.toml 里手写的规则)。
    #[serde(default)]
    pub exact: bool,
}

fn star() -> String {
    "*".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleSet {
    #[serde(default)]
    pub allow: Vec<Rule>,
    #[serde(default)]
    pub deny: Vec<Rule>,
}

impl RuleSet {
    pub fn load() -> Self {
        let p = crate::config::Config::permissions_path();
        match std::fs::read_to_string(&p) {
            Ok(s) => match toml::from_str(&s) {
                Ok(rs) => rs,
                Err(e) => {
                    // 静默吞掉解析错误会让用户误以为规则仍然生效——大声报出来。
                    tracing::error!("permissions.toml 解析失败,已忽略全部规则: {e}");
                    Self::default()
                }
            },
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                tracing::error!("permissions.toml 读取失败,已忽略全部规则: {e}");
                Self::default()
            }
            // 首次运行没有规则文件是正常情况
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> crate::Result<()> {
        let p = crate::config::Config::permissions_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let s = toml::to_string_pretty(self).map_err(|e| HarnessError::Config(e.to_string()))?;
        std::fs::write(p, s)?;
        Ok(())
    }

    fn matches(rule: &Rule, tool: &str, path: &str, command: Option<&str>) -> bool {
        if rule.tool != "*" && rule.tool != tool {
            return false;
        }
        if rule.exact {
            return match command {
                // shell 类规则存命令原文,按命令等值比较(可复现、无通配惊喜)
                Some(cmd) => rule.path == cmd,
                // 路径类规则存路径原文;Windows 文件系统大小写不敏感
                None => path_exact(&rule.path, path),
            };
        }
        // 老式 glob 规则(用户在 permissions.toml 手写)。注意 bash 的 path 是
        // cwd:拿它做 glob 放行等于"该目录下任意命令永久免确认"——这正是要堵
        // 掉的洞,所以非 exact 的 bash 规则一律不放行(保留 `*` 全局语义)。
        if rule.tool == "bash" {
            return rule.path == "*";
        }
        rule.path == "*" || glob_match(&rule.path, path)
    }

    pub fn lookup(&self, tool: &str, path: &str, command: Option<&str>) -> Option<bool> {
        if self
            .deny
            .iter()
            .any(|r| Self::matches(r, tool, path, command))
        {
            return Some(false);
        }
        if self
            .allow
            .iter()
            .any(|r| Self::matches(r, tool, path, command))
        {
            return Some(true);
        }
        None
    }
}

/// 路径精确比较(非 shell 类的 exact 规则):Windows 上大小写不敏感。
fn path_exact(rule_path: &str, path: &str) -> bool {
    if cfg!(windows) {
        rule_path.eq_ignore_ascii_case(path)
    } else {
        rule_path == path
    }
}

/// Glob matching via `globset` (`*`, `?`, `**`, `[...]`), matched against the
/// full path and, for convenience, against the basename too.
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    use globset::{Glob, GlobMatcher};
    let glob = match Glob::new(pattern) {
        Ok(g) => g,
        Err(_) => return pattern == text,
    };
    let m: GlobMatcher = glob.compile_matcher();
    if m.is_match(text) {
        return true;
    }
    // Also match when the pattern has no separator: treat it as "any dir".
    if !pattern.contains('/') && !pattern.contains('\\') {
        if let Some(name) = Path::new(text).file_name().and_then(|n| n.to_str()) {
            return m.is_match(name);
        }
    }
    false
}

/// dunce 风格的轻量规范化:剥离 canonicalize 在 Windows 上引入的 `\\?\` /
/// `\\?\UNC\` 前缀,统一两边形态,避免 `\\?\C:\...` 与 `C:\...` 前缀失配。
fn strip_verbatim(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{}", rest)
    } else if let Some(rest) = s.strip_prefix("\\\\?\\") {
        rest.to_string()
    } else {
        s.into_owned()
    }
}

/// 组件级归一化:丢掉 `.`、按组件弹出 `..`,再以字符串组件做前缀比较。
/// 组件级比较天然保证 workspace 边界落在分隔符上("foo" 不会前缀匹配 "foobar")。
fn clean_components(p: &Path) -> Vec<String> {
    let cleaned = strip_verbatim(p);
    let mut out = Vec::new();
    for comp in Path::new(&cleaned).components() {
        use std::path::Component::*;
        match comp {
            CurDir => {}
            ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str().to_string_lossy().to_string()),
        }
    }
    out
}

/// 组件级前缀比较:Windows 下大小写不敏感(NTFS/Win32 语义)。
fn norm_prefix(workspace: &Path, path: &Path) -> bool {
    let w = clean_components(workspace);
    if w.is_empty() {
        // 归一化后为空的工作区(比如 ".")没有边界可言,一律不算内部。
        return false;
    }
    let p = clean_components(path);
    if p.len() < w.len() {
        return false;
    }
    let win = cfg!(windows);
    w.iter()
        .zip(p.iter())
        .all(|(a, b)| a == b || (win && a.eq_ignore_ascii_case(b)))
}

/// Is `path` inside `workspace`?
/// 两边都先尝试 canonicalize(统一剥 `\\?\` 前缀),再按组件做前缀比较;
/// 不存在的路径退回逻辑组件归一化——`..` 被真正弹出,而不是靠字符串前缀蒙混。
pub fn inside_workspace(workspace: &Path, path: &Path) -> bool {
    let w_real = workspace.canonicalize().ok();
    let p_real = path.canonicalize().ok();
    norm_prefix(
        w_real.as_deref().unwrap_or(workspace),
        p_real.as_deref().unwrap_or(path),
    )
}

/// `~/.parable` 内部(config.toml / permissions.toml / sessions / logs / uploads
/// / plugins / schedule.json…)是 harness 自身的私密目录。只读工具一律不碰,
/// 工具层(authorize)据此直接报错,不弹确认面板——防凭据/会话泄露与提示词注入。
/// 这是"不命中即放行"的检查,所以逻辑形态与 canonicalize 形态任一命中都算内部,
/// 防 8.3 短名(C:\Users\EASON~1\)与符号链接形态差异导致的漏判。
pub fn is_parable_internal(path: &Path) -> bool {
    let root = crate::config::Config::home_dir();
    let root_real = root.canonicalize().unwrap_or_else(|_| root.clone());
    if norm_prefix(&root, path) || norm_prefix(&root_real, path) {
        return true;
    }
    match path.canonicalize() {
        Ok(p) => norm_prefix(&root, &p) || norm_prefix(&root_real, &p),
        Err(_) => false,
    }
}

pub struct PendingRequest {
    pub tx: oneshot::Sender<Decision>,
    pub tool: String,
    pub path: String,
    /// `Risk::as_str()` 的静态串,供 GET /api/pending 复原面板。
    pub risk: &'static str,
    pub reason: String,
    pub diff_preview: Option<String>,
    /// The session this request belongs to. Two conversations can run in
    /// parallel (the side pane's side chat is a separate session), so the
    /// request must carry its own session instead of relying on a global.
    pub session_id: String,
}

/// Owns the rule table and the set of in-flight permission requests.
pub struct PermissionBroker {
    rules: RwLock<RuleSet>,
    pending: Mutex<HashMap<String, PendingRequest>>,
    hub: EventHub,
    /// 每个 call 的权限判定来源("auto"=规则/模式放行,"requested"=用户决策),
    /// 供 Registry::execute 的执行审计取用。
    sources: Mutex<HashMap<String, &'static str>>,
}

impl PermissionBroker {
    pub fn new(hub: EventHub) -> Self {
        Self {
            rules: RwLock::new(RuleSet::load()),
            pending: Mutex::new(HashMap::new()),
            hub,
            sources: Mutex::new(HashMap::new()),
        }
    }

    /// 取消所有挂起的权限请求：turn 被终止时，等待用户点“允许/拒绝”的
    /// 工具 future 已被 drop，但 pending 表若不清理，重连后仍会弹旧面板。
    pub async fn cancel_all(&self) {
        self.pending.lock().await.clear();
    }

    /// Decide whether `tool` acting on `path` may run without asking.
    ///
    /// - 规则表(deny 优先)永远先行;Always 决策落成 exact 规则(shell 类存
    ///   命令原文),可精确复现,不再有 cwd/glob 的连带放行。
    /// - full:用户显式选择的全放行(含破坏性操作,那是用户自己的选择)。
    /// - ws:只读工具限工作区内;shell / 网络 / 破坏性操作一律送确认(README
    ///   语义:区外写入、删除、shell、网络仍确认)。
    /// - ask:只读工具限工作区内(区外读取送确认),其余一律送确认。
    pub async fn auto_allow(
        &self,
        mode: &str,
        tool: &str,
        path: &str,
        command: Option<&str>,
        risk: Risk,
        workspace: &Path,
    ) -> bool {
        let rules = self.rules.read().await;
        match rules.lookup(tool, path, command) {
            Some(true) => true,
            Some(false) => false,
            None => match mode {
                "full" => true,
                "ws" => {
                    if matches!(tool, "read_file" | "glob" | "grep" | "list_dir") {
                        return inside_workspace(workspace, Path::new(path));
                    }
                    // shell 类(bash 与插件命令,均为 Risk::Shell)、网络、破坏性
                    // 操作即便在工作区内也要走确认。
                    !matches!(risk, Risk::Shell | Risk::Network | Risk::Destructive)
                        && inside_workspace(workspace, Path::new(path))
                }
                _ => {
                    // ask:只读工具也不再全局放行,区外路径送确认。
                    matches!(tool, "read_file" | "glob" | "grep" | "list_dir")
                        && inside_workspace(workspace, Path::new(path))
                }
            },
        }
    }

    /// Suspend until the user answers. Emits `permission.request` and waits.
    /// `command` 是 shell 类工具渲染后的命令原文(bash / 插件命令),用于 Always
    /// 规则的精确持久化;非 shell 工具传 None。
    /// `session_id` 是发起这次调用的会话:权限请求按会话投递,两个会话并行时
    /// 不会串台。
    #[allow(clippy::too_many_arguments)]
    pub async fn request(
        &self,
        call_id: &str,
        session_id: &str,
        tool: &str,
        path: &str,
        risk: Risk,
        reason: &str,
        diff_preview: Option<String>,
        command: Option<&str>,
    ) -> Result<Decision, HarnessError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().await;
            p.insert(
                call_id.to_string(),
                PendingRequest {
                    tx,
                    tool: tool.to_string(),
                    path: path.to_string(),
                    risk: risk.as_str(),
                    reason: reason.to_string(),
                    diff_preview: diff_preview.clone(),
                    session_id: session_id.to_string(),
                },
            );
        }
        self.hub.publish(
            session_id,
            "permission.request",
            json!({
                "callId": call_id,
                "tool": tool,
                "path": path,
                "risk": risk.as_str(),
                "reason": reason,
                "diffPreview": diff_preview,
            }),
        );

        // Timeout: never hang the agent forever. 超时按拒绝处理,并且与用户决策
        // 一样发 permission.resolved,前端面板才能收尾。
        let decision = tokio::select! {
            res = rx => res.unwrap_or(Decision::Deny),
            _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => Decision::Deny,
        };

        self.pending.lock().await.remove(call_id);
        self.sources
            .lock()
            .await
            .insert(call_id.to_string(), "requested");

        if decision == Decision::Always {
            let mut rules = self.rules.write().await;
            // Always 规则必须可精确复现:shell 类存命令原文等值比较,其余存路径
            // 精确比较。绝不把 cwd/glob 这类可变参数持久成放行依据。
            rules.allow.push(Rule {
                tool: tool.to_string(),
                path: command.unwrap_or(path).to_string(),
                action: "allow".into(),
                exact: true,
            });
            if let Err(e) = rules.save() {
                tracing::error!("权限规则持久化失败: {e}");
            }
        }
        self.hub.publish(
            session_id,
            "permission.resolved",
            json!({"id": call_id, "decision": match decision {
                Decision::Once => "once",
                Decision::Always => "always",
                Decision::Deny => "deny",
            }}),
        );
        self.audit(call_id, tool, path, risk, decision).await;
        Ok(decision)
    }

    /// Answer a pending request from the HTTP layer.
    pub async fn resolve(&self, call_id: &str, decision: Decision) -> bool {
        let tx = self.pending.lock().await.remove(call_id).map(|p| p.tx);
        match tx {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// 当前挂起的权限请求(供 GET /api/pending 在断线重连后恢复面板)。
    /// 形状与 permission.request 事件字段一致,`id` 即事件的 `callId`。
    pub async fn pending_list(&self) -> Vec<serde_json::Value> {
        let p = self.pending.lock().await;
        let mut out: Vec<serde_json::Value> = p
            .iter()
            .map(|(id, r)| {
                json!({
                    "id": id,
                    "tool": r.tool,
                    "path": r.path,
                    "risk": r.risk,
                    "reason": r.reason,
                    "diffPreview": r.diff_preview,
                    "sessionId": r.session_id,
                })
            })
            .collect();
        // 稳定输出顺序(先到先得),方便前端恢复面板。
        out.sort_by(|a, b| {
            a.get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("id").and_then(|v| v.as_str()).unwrap_or(""))
        });
        out
    }

    pub async fn rules(&self) -> RuleSet {
        self.rules.read().await.clone()
    }

    /// 记录一次权限判定来源,供执行审计标注("auto" / "requested")。
    pub async fn note_decision(&self, call_id: &str, source: &'static str) {
        self.sources
            .lock()
            .await
            .insert(call_id.to_string(), source);
    }

    /// 工具执行审计用:该 call 的判定来源。没走过权限门的内置安全工具默认 "auto"。
    pub async fn decision_of(&self, call_id: &str) -> &'static str {
        self.sources
            .lock()
            .await
            .get(call_id)
            .copied()
            .unwrap_or("auto")
    }

    /// auto_allow 放行(rules / ws / full)也落一条审计,decision 标 "auto"。
    pub async fn audit_auto(&self, call_id: &str, tool: &str, path: &str, risk: Risk) {
        self.write_audit_line(json!({
            "ts": crate::event::now_ms(),
            "callId": call_id,
            "tool": tool,
            "path": path,
            "risk": risk.as_str(),
            "decision": "auto",
        }));
    }

    /// 工具执行审计:tool 名、判定来源、耗时;无论成败都记一条。
    pub async fn audit_execution(
        &self,
        call_id: &str,
        tool: &str,
        source: &str,
        ok: bool,
        elapsed_ms: u128,
    ) {
        self.write_audit_line(json!({
            "ts": crate::event::now_ms(),
            "callId": call_id,
            "tool": tool,
            "event": "execute",
            "source": source,
            "ok": ok,
            "elapsedMs": elapsed_ms.min(u64::MAX as u128) as u64,
        }));
    }

    fn write_audit_line(&self, line: serde_json::Value) {
        let dir = crate::config::Config::logs_dir();
        let _ = std::fs::create_dir_all(&dir);
        let day = chrono::Local::now().format("%Y%m%d").to_string();
        let file = dir.join(format!("audit-{day}.jsonl"));
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
            let _ = writeln!(f, "{line}");
        }
    }

    async fn audit(&self, call_id: &str, tool: &str, path: &str, risk: Risk, d: Decision) {
        self.write_audit_line(json!({
            "ts": crate::event::now_ms(),
            "callId": call_id,
            "tool": tool,
            "path": path,
            "risk": risk.as_str(),
            "decision": match d { Decision::Once => "once", Decision::Always => "always", Decision::Deny => "deny" },
        }));
    }
}

pub type SharedPermissions = Arc<PermissionBroker>;

#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<PermissionBroker>();
    let _: PathBuf = crate::config::Config::home_dir();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_exact_command_match() {
        let rs = RuleSet {
            allow: vec![Rule {
                tool: "bash".into(),
                path: "cargo build".into(),
                action: "allow".into(),
                exact: true,
            }],
            deny: vec![],
        };
        assert_eq!(rs.lookup("bash", "C:\\ws", Some("cargo build")), Some(true));
        // 命令不完全相等就不放行
        assert_eq!(rs.lookup("bash", "C:\\ws", Some("cargo build --release")), None);
        assert_eq!(rs.lookup("bash", "C:\\ws", Some("cargo clean")), None);
    }

    #[test]
    fn legacy_bash_path_rules_no_longer_blast_wide() {
        // 老版本 Always 会把 cwd 存成 path:这类规则对 bash 一律失效
        let rs = RuleSet {
            allow: vec![Rule {
                tool: "bash".into(),
                path: "C:\\ws".into(),
                action: "allow".into(),
                exact: false,
            }],
            deny: vec![],
        };
        assert_eq!(rs.lookup("bash", "C:\\ws", Some("rm -rf /")), None);
        // 但全局语义仍保留
        let rs_star = RuleSet {
            allow: vec![Rule {
                tool: "bash".into(),
                path: "*".into(),
                action: "allow".into(),
                exact: false,
            }],
            deny: vec![],
        };
        assert_eq!(rs_star.lookup("bash", "C:\\ws", Some("ls")), Some(true));
    }

    #[test]
    fn exact_path_rule_is_literal() {
        let rs = RuleSet {
            allow: vec![Rule {
                tool: "write_file".into(),
                path: "C:\\ws\\a*b.txt".into(),
                action: "allow".into(),
                exact: true,
            }],
            deny: vec![],
        };
        // exact 规则不做 glob 展开:星号是字面量
        assert_eq!(rs.lookup("write_file", "C:\\ws\\a*b.txt", None), Some(true));
        assert_eq!(rs.lookup("write_file", "C:\\ws\\aXb.txt", None), None);
    }

    #[test]
    fn deny_precedes_allow() {
        let rs = RuleSet {
            allow: vec![Rule {
                tool: "*".into(),
                path: "*".into(),
                action: "allow".into(),
                exact: false,
            }],
            deny: vec![Rule {
                tool: "bash".into(),
                path: "*".into(),
                action: "deny".into(),
                exact: false,
            }],
        };
        assert_eq!(rs.lookup("bash", "x", None), Some(false));
        assert_eq!(rs.lookup("read_file", "x", None), Some(true));
    }

    #[test]
    fn inside_ws_component_boundary() {
        // 用临时目录保证判定与机器上恰好存在的路径无关(组件级,不靠字符串前缀);
        // base 先 canonicalize 统一形态,避开 Windows 8.3 短名(TEMP 可能是 EASON~1)
        let base = std::env::temp_dir().join(format!("parable-ws-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).ok();
        let base = base.canonicalize().unwrap_or(base);
        let w = base.join("ws");
        // 组件边界:wsfoo 不在 ws/ 前缀里
        assert!(inside_workspace(&w, &w.join("sub/f.txt")));
        assert!(!inside_workspace(&w, &base.join("wsfoo/f.txt")));
        // `..` 在组件级被真正弹出
        assert!(inside_workspace(
            &w,
            &w.join("sub").join("..").join("f.txt")
        ));
        assert!(!inside_workspace(
            &w,
            &w.join("..").join("evil").join("f.txt")
        ));
        assert!(!inside_workspace(&w, &base.parent().unwrap().join("elsewhere/f.txt")));
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(windows)]
    #[test]
    fn inside_ws_windows_case_insensitive() {
        let base = std::env::temp_dir().join(format!("parable-ws-ci-{}", std::process::id()));
        std::fs::create_dir_all(&base).ok();
        let base = base.canonicalize().unwrap_or(base);
        let w = base.join("Work").join("Ws");
        std::fs::create_dir_all(&w).ok();
        // 存在的目录两边都会 canonicalize,比较仍要大小写不敏感
        assert!(inside_workspace(
            &w,
            &base.join("work").join("ws").join("sub").join("f.txt")
        ));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn parable_internal_detection() {
        let root = crate::config::Config::home_dir();
        assert!(is_parable_internal(&root.join("config.toml")));
        assert!(is_parable_internal(&root.join("uploads").join("x.png")));
        // 组件级 `..` 被弹出,不再落在 ~/.parable 下
        assert!(!is_parable_internal(&root.join("..")));
        // 根自身也算内部
        assert!(is_parable_internal(&root));
    }
}
