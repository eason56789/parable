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
    /// 命令模式（Claude Code 的 `Bash(npm:*)` 同款）：设置后本规则只作用于
    /// shell 类工具的命令串（glob 匹配），path 字段被忽略。这让
    /// `bash + cmd="npm *"` 可以安全地放行"以 npm 开头的命令"，而不会像
    /// 老式 path-glob 那样放行"该目录下任意命令"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
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
    /// ask 规则：即使 full / ws 模式也强制走确认流程（deny 优先级最高，
    /// ask 次之，allow 最后）。手写规则用它的典型场景："全局 full，但
    /// bash rm 必须问我"。
    #[serde(default)]
    pub ask: Vec<Rule>,
}

/// 规则判定结果（deny > ask > allow）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleOutcome {
    Allow,
    Deny,
    Ask,
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
        // 命令模式规则：只看命令串（Claude Code Bash(npm:*) 语义的安全版）。
        // 与 path 互斥——cmd 存在时 path 不参与判定。
        if let Some(pat) = &rule.cmd {
            return match command {
                Some(c) if !c.is_empty() => glob_match(pat, c),
                _ => false,
            };
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

    /// 三态判定：deny > ask > allow；无匹配规则返回 None。auto_allow 用它
    /// 替代旧的二值 lookup。
    pub fn decide(&self, tool: &str, path: &str, command: Option<&str>) -> Option<RuleOutcome> {
        if self
            .deny
            .iter()
            .any(|r| Self::matches(r, tool, path, command))
        {
            return Some(RuleOutcome::Deny);
        }
        if self
            .ask
            .iter()
            .any(|r| Self::matches(r, tool, path, command))
        {
            return Some(RuleOutcome::Ask);
        }
        if self
            .allow
            .iter()
            .any(|r| Self::matches(r, tool, path, command))
        {
            return Some(RuleOutcome::Allow);
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

/// Is `path` inside the workspace or one of the extra allowed roots
/// (CLI `--add-dir`)?
pub fn inside_any(workspace: &Path, roots: &[PathBuf], path: &Path) -> bool {
    inside_workspace(workspace, path) || roots.iter().any(|r| inside_workspace(r, path))
}

/// C-12：组件级前缀比较（不做 canonicalize），供 fs 层与网关共用同一套
/// Windows 大小写不敏感口径，避免两边对 `Foo`/`foo` 判定分叉。
pub fn lexical_inside(parent: &Path, child: &Path) -> bool {
    norm_prefix(parent, child)
}

// ------------------------------------------------------------------ 阶段四·沙箱 v1

/// 沙箱判定结果。`off` 模式下恒为 [`SandboxVerdict::Allow`]，既有行为不变。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxVerdict {
    /// 放行：off 模式，或检查通过。
    Allow,
    /// warn 模式：越界/可疑，只在结果里标注，不阻断执行。
    Warn { notes: Vec<String> },
    /// enforce 模式：越权，结构化拒绝（code + 说明 + 建议），不弹审批。
    Deny {
        code: String,
        message: String,
        suggestion: String,
    },
}

impl SandboxVerdict {
    pub fn notes(&self) -> Vec<String> {
        match self {
            SandboxVerdict::Warn { notes } => notes.clone(),
            _ => Vec::new(),
        }
    }
}

/// 沙箱策略：从配置快照构造。默认 `off`，与改前行为完全一致。
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub mode: String,
    pub network_allow: Vec<String>,
}

impl SandboxPolicy {
    pub fn off() -> Self {
        Self {
            mode: "off".to_string(),
            network_allow: Vec::new(),
        }
    }

    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            mode: cfg.sandbox.mode.clone(),
            network_allow: cfg.sandbox.network_allow.clone(),
        }
    }

    pub fn is_off(&self) -> bool {
        !matches!(self.mode.as_str(), "warn" | "enforce")
    }

    /// 写目标边界检查：命令前置声明的 cwd 必须落在 workspace 根或追加目录内。
    /// 用「解析绝对路径 + canonicalize 后组件前缀比较」，不靠正则拆命令。
    pub fn check_cwd(&self, workspace: &Path, roots: &[PathBuf], cwd: &Path) -> SandboxVerdict {
        if self.is_off() {
            return SandboxVerdict::Allow;
        }
        if inside_any(workspace, roots, cwd) {
            return SandboxVerdict::Allow;
        }
        let message = format!(
            "工作目录 `{}` 越出当前会话工作区根 `{}`",
            cwd.display(),
            workspace.display()
        );
        self.deny_or_warn(
            "sandbox:write-escape",
            message,
            "把命令的 cwd 放在当前会话工作区目录内；确需访问区外路径时，在配置中把 sandbox.mode 调为 off。".into(),
        )
    }

    /// 命令内显式写出的绝对路径（保守抽取，仅做边界比对；拆不出的命令不臆测）。
    pub fn check_command_paths(&self, workspace: &Path, roots: &[PathBuf], command: &str) -> SandboxVerdict {
        if self.is_off() {
            return SandboxVerdict::Allow;
        }
        let mut notes = Vec::new();
        for token in shell_tokens(command) {
            let Some(p) = abs_path_token(&token) else { continue };
            if !inside_any(workspace, roots, &p) {
                notes.push(format!(
                    "命令引用的路径 `{}` 越出工作区根 `{}`",
                    p.display(),
                    workspace.display()
                ));
            }
        }
        if notes.is_empty() {
            SandboxVerdict::Allow
        } else {
            self.deny_or_warn("sandbox:path-escape", notes.join("；"),
                "命令内的区外绝对路径请改为工作区相对路径；或在配置中关闭沙箱。".into())
        }
    }

    /// 主机是否在网络白名单（域名后缀 / IP 前缀，大小写不敏感，忽略端口）。
    pub fn host_allowed(&self, host: &str) -> bool {
        let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
        if host.is_empty() {
            return false;
        }
        self.network_allow.iter().any(|entry| {
            let a = entry.trim().to_ascii_lowercase();
            if a.is_empty() {
                return false;
            }
            if a.ends_with('.') {
                // 网段前缀写法（如 `10.0.0.`）：主机以前缀开头即放行。
                host.starts_with(&a)
            } else {
                host == a || host.ends_with(&format!(".{a}"))
            }
        })
    }

    /// 出站网络预检：识别常见联网动词并抽取目标主机。enforce 下主机不在
    /// 白名单即拒绝；warn 下仅标注。注意这是命令行预检，真正的子进程 socket
    /// 隔离需 OS 级 job object / 防火墙 / 代理，属后续路线图。
    pub fn check_network_command(&self, command: &str) -> SandboxVerdict {
        if self.is_off() {
            return SandboxVerdict::Allow;
        }
        let tokens: Vec<String> = shell_tokens(command);
        let Some(verb) = tokens.first().map(|t| t.to_ascii_lowercase()) else {
            return SandboxVerdict::Allow;
        };
        const NETWORK_VERBS: [&str; 12] = [
            "curl", "curl.exe", "wget", "wget.exe", "ssh", "scp", "sftp", "ftp",
            "nc", "ncat", "telnet", "invoke-webrequest",
        ];
        if !NETWORK_VERBS.contains(&verb.as_str()) {
            return SandboxVerdict::Allow;
        }
        let host = tokens
            .iter()
            .skip(1)
            .find_map(|t| url_host(t))
            .unwrap_or_default();
        if host.is_empty() {
            // 联网动词但抽不出主机：拿不准，按可疑处理（warn 标注 / enforce 拒绝）。
            return self.deny_or_warn(
                "sandbox:network-unknown-host",
                format!("命令以联网工具 `{verb}` 开头，但未能解析目标主机"),
                "在 sandbox.network_allow 中加入目标域名/IP，或关闭沙箱。".into(),
            );
        }
        if self.host_allowed(&host) {
            return SandboxVerdict::Allow;
        }
        self.deny_or_warn(
            "sandbox:network-denied",
            format!("联网目标 `{host}` 不在沙箱网络白名单内"),
            format!("把 `{host}` 加入配置 sandbox.network_allow，或在配置中关闭沙箱。"),
        )
    }

    fn deny_or_warn(&self, code: &str, message: String, suggestion: String) -> SandboxVerdict {
        match self.mode.as_str() {
            "enforce" => SandboxVerdict::Deny {
                code: code.to_string(),
                message,
                suggestion,
            },
            // warn（以及任何非法 mode 值）：只标注，不阻断。
            _ => SandboxVerdict::Warn {
                notes: vec![format!("[{code}] {message}")],
            },
        }
    }
}

/// 粗切 shell 词元：按空白与常见分隔符拆分，剥掉成对引号。仅用于路径/主机预检。
fn shell_tokens(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for c in command.chars() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 词元是否是绝对路径：Windows 盘符 / UNC，或 Unix 首斜杠。
fn abs_path_token(token: &str) -> Option<PathBuf> {
    let t = token.trim_matches(|c| c == '\'' || c == '"' || c == ',' || c == ';');
    let win = t
        .strip_prefix("\\\\")
        .map(|_| t.to_string())
        .or_else(|| {
            let mut chars = t.chars();
            let drive = chars.next()?;
            if drive.is_ascii_alphabetic() && chars.next() == Some(':') {
                Some(t.to_string())
            } else {
                None
            }
        });
    if let Some(w) = win {
        return Some(PathBuf::from(w));
    }
    if t.starts_with('/') {
        Some(PathBuf::from(t))
    } else {
        None
    }
}

/// 从词元里取主机：剥 scheme、认证信息、路径与端口。
/// 裸单词（无 scheme、无点号）视为命令名/本地词，返回 None。
fn url_host(token: &str) -> Option<String> {
    let t = token.trim_matches(|c| c == '\'' || c == '"' || c == ',');
    let has_scheme = t.starts_with("https://") || t.starts_with("http://");
    let t = t.strip_prefix("https://").or_else(|| t.strip_prefix("http://")).unwrap_or(t);
    let t = t.split('/').next().unwrap_or(t);
    let t = t.split('?').next().unwrap_or(t);
    let t = t.split('#').next().unwrap_or(t);
    let t = t.rsplit('@').next().unwrap_or(t);
    let host = t.split(':').next().unwrap_or(t);
    if host.is_empty() || host.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    if !has_scheme && !host.contains('.') {
        return None;
    }
    Some(host.to_string())
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
        match rules.decide(tool, path, command) {
            Some(RuleOutcome::Allow) => true,
            // deny 与 ask 都走确认流：deny 的语义是"拒绝放行"，交给调用方
            // 以确认框呈现（用户明确点允许仍可执行一次）；ask 则是"规则说
            // 必须问"——即使在 full 模式下也要问。
            Some(RuleOutcome::Deny) | Some(RuleOutcome::Ask) => false,
            None => match mode {
                "full" => true,
                "ws" => {
                    if matches!(tool, "read_file" | "glob" | "grep" | "list_dir" | "doc_read") {
                        return inside_workspace(workspace, Path::new(path));
                    }
                    // shell 类(bash 与插件命令,均为 Risk::Shell)、网络、破坏性
                    // 操作即便在工作区内也要走确认。
                    !matches!(risk, Risk::Shell | Risk::Network | Risk::Destructive)
                        && inside_workspace(workspace, Path::new(path))
                }
                _ => {
                    // ask:只读工具也不再全局放行,区外路径送确认。
                    matches!(tool, "read_file" | "glob" | "grep" | "list_dir" | "doc_read")
                        && inside_workspace(workspace, Path::new(path))
                }
            },
        }
    }

    /// C-07：规则表命中 `deny` 时硬阻断——不进确认流、面板上点「允许一次」
    /// 也不能覆盖。`ask`/`allow`/未命中仍走 `auto_allow` 的既有流程。
    pub async fn rule_denied(&self, tool: &str, path: &str, command: Option<&str>) -> bool {
        matches!(
            self.rules.read().await.decide(tool, path, command),
            Some(RuleOutcome::Deny)
        )
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
                cmd: None,
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
                cmd: None,
            }],
            deny: vec![],
            ask: vec![],
        };
        assert_eq!(rs.decide("bash", "C:\\ws", Some("cargo build")), Some(RuleOutcome::Allow));
        // 命令不完全相等就不放行
        assert_eq!(rs.decide("bash", "C:\\ws", Some("cargo build --release")), None);
        assert_eq!(rs.decide("bash", "C:\\ws", Some("cargo clean")), None);
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
                cmd: None,
            }],
            deny: vec![],
            ask: vec![],
        };
        assert_eq!(rs.decide("bash", "C:\\ws", Some("rm -rf /")), None);
        // 但全局语义仍保留
        let rs_star = RuleSet {
            allow: vec![Rule {
                tool: "bash".into(),
                path: "*".into(),
                action: "allow".into(),
                exact: false,
                cmd: None,
            }],
            deny: vec![],
            ask: vec![],
        };
        assert_eq!(rs_star.decide("bash", "C:\\ws", Some("ls")), Some(RuleOutcome::Allow));
    }

    #[test]
    fn exact_path_rule_is_literal() {
        let rs = RuleSet {
            allow: vec![Rule {
                tool: "write_file".into(),
                path: "C:\\ws\\a*b.txt".into(),
                action: "allow".into(),
                exact: true,
                cmd: None,
            }],
            deny: vec![],
            ask: vec![],
        };
        // exact 规则不做 glob 展开:星号是字面量
        assert_eq!(rs.decide("write_file", "C:\\ws\\a*b.txt", None), Some(RuleOutcome::Allow));
        assert_eq!(rs.decide("write_file", "C:\\ws\\aXb.txt", None), None);
    }

    #[test]
    fn deny_precedes_allow() {
        let rs = RuleSet {
            allow: vec![Rule {
                tool: "*".into(),
                path: "*".into(),
                action: "allow".into(),
                exact: false,
                cmd: None,
            }],
            deny: vec![Rule {
                tool: "bash".into(),
                path: "*".into(),
                action: "deny".into(),
                exact: false,
                cmd: None,
            }],
            ask: vec![],
        };
        assert_eq!(rs.decide("bash", "x", None), Some(RuleOutcome::Deny));
        assert_eq!(rs.decide("read_file", "x", None), Some(RuleOutcome::Allow));
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

    #[test]
    fn inside_any_admits_workspace_and_extra_roots_only() {
        let base = std::env::temp_dir().join(format!("parable-any-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).ok();
        let base = base.canonicalize().unwrap_or(base);
        let w = base.join("ws");
        let extra = base.join("extra");
        let other = base.join("other");
        let roots = vec![extra.clone()];
        assert!(inside_any(&w, &roots, &w.join("f.txt")));
        assert!(inside_any(&w, &roots, &extra.join("sub/f.txt")));
        assert!(!inside_any(&w, &roots, &other.join("f.txt")));
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

    fn scratch_pair(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let ws = std::env::temp_dir().join(format!("parable-sbx-{tag}-ws-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("parable-sbx-{tag}-out-{}", std::process::id()));
        std::fs::create_dir_all(&ws).ok();
        std::fs::create_dir_all(&outside).ok();
        (ws, outside)
    }

    #[test]
    fn sandbox_off_allows_escape_unchanged() {
        let (ws, outside) = scratch_pair("off");
        let policy = SandboxPolicy { mode: "off".into(), network_allow: Vec::new() };
        // off：既有行为完全不变，越界也放行。
        assert!(matches!(policy.check_cwd(&ws, &[], &outside), SandboxVerdict::Allow));
    }

    #[test]
    fn sandbox_enforce_denies_write_escape_with_structured_error() {
        let (ws, outside) = scratch_pair("deny");
        let policy = SandboxPolicy { mode: "enforce".into(), network_allow: Vec::new() };
        match policy.check_cwd(&ws, &[], &outside) {
            SandboxVerdict::Deny { code, message, suggestion } => {
                assert_eq!(code, "sandbox:write-escape");
                assert!(!message.is_empty());
                assert!(!suggestion.is_empty());
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn sandbox_warn_marks_but_does_not_block() {
        let (ws, outside) = scratch_pair("warn");
        let policy = SandboxPolicy { mode: "warn".into(), network_allow: Vec::new() };
        match policy.check_cwd(&ws, &[], &outside) {
            SandboxVerdict::Warn { notes } => assert!(!notes.is_empty()),
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn sandbox_enforce_allows_extra_root_but_denies_other_outside() {
        let base = std::env::temp_dir().join(format!("parable-sbx-roots-{}", std::process::id()));
        let ws = base.join("ws");
        let extra = base.join("extra");
        let other = base.join("other");
        std::fs::create_dir_all(&ws).ok();
        std::fs::create_dir_all(&extra).ok();
        std::fs::create_dir_all(&other).ok();
        let roots = vec![extra.clone()];
        let policy = SandboxPolicy { mode: "enforce".into(), network_allow: Vec::new() };
        // 追加目录内的 cwd 放行（与 --workspace 不冲突）。
        assert!(matches!(policy.check_cwd(&ws, &roots, &extra), SandboxVerdict::Allow));
        // 负向：既不在工作区、也不在追加目录 → 拒绝。
        match policy.check_cwd(&ws, &roots, &other) {
            SandboxVerdict::Deny { .. } => {}
            other => panic!("expected Deny, got {other:?}"),
        }
        // 命令路径同理：追加目录内放行，区外拒绝。
        let cmd_ok = format!("ls {}", extra.join("f.txt").to_string_lossy());
        assert!(matches!(
            policy.check_command_paths(&ws, &roots, &cmd_ok),
            SandboxVerdict::Allow
        ));
        let cmd_bad = format!("cat {}", other.join("f.txt").to_string_lossy());
        assert!(matches!(
            policy.check_command_paths(&ws, &roots, &cmd_bad),
            SandboxVerdict::Deny { .. }
        ));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn sandbox_network_allowlist_allows_only_listed_hosts() {
        let policy = SandboxPolicy {
            mode: "enforce".into(),
            network_allow: vec!["api.example.com".into(), "10.0.0.".into()],
        };
        assert!(policy.host_allowed("api.example.com"));
        assert!(policy.host_allowed("sub.api.example.com"));
        assert!(policy.host_allowed("10.0.0.5"));
        assert!(!policy.host_allowed("evil.com"));

        match policy.check_network_command("curl https://evil.com/x") {
            SandboxVerdict::Deny { code, .. } => assert_eq!(code, "sandbox:network-denied"),
            other => panic!("expected Deny, got {other:?}"),
        }
        assert!(matches!(
            policy.check_network_command("curl https://api.example.com/v1"),
            SandboxVerdict::Allow
        ));
    }

}
