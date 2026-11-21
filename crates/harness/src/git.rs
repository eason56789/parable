//! Git integration for the side pane's review tab.
//!
//! The harness had no git support at all — the model could run `git diff`
//! through the shell tool, but nothing parsed the output. This module shells
//! out to the `git` binary and returns structured status/diff data.
//!
//! Everything runs with `current_dir` pinned to the workspace and with
//! `-c core.quotepath=false` so paths are readable rather than escaped.

use crate::error::HarnessError;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// git stdout 最大保留字节数（G-02）：超出截断，防超大 diff/status 撑爆内存。
const MAX_GIT_OUTPUT: usize = 2 * 1024 * 1024;

/// One changed file.
#[derive(Debug, Clone, Serialize)]
pub struct FileStatus {
    /// Path relative to the repository root, using `/` separators.
    pub path: String,
    /// Two-letter porcelain v2 code, e.g. `1.` (modified), `?` (untracked).
    pub code: String,
    /// Human-readable state: `modified` / `added` / `deleted` / `renamed` / `untracked`.
    pub state: String,
    /// `true` when the change is staged for commit.
    pub staged: bool,
}

/// Working-tree status of the repository.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// `false` when the workspace is not inside a git repository.
    pub is_repo: bool,
    /// Current branch name (`HEAD` when detached).
    pub branch: String,
    pub files: Vec<FileStatus>,
}

/// One line inside a diff hunk.
#[derive(Debug, Clone, Serialize)]
pub struct DiffLine {
    /// `+`, `-` or ` `.
    pub sign: char,
    pub text: String,
}

/// A contiguous region of change.
#[derive(Debug, Clone, Serialize)]
pub struct Hunk {
    pub header: String,
    pub old_start: u64,
    pub old_lines: u64,
    pub new_start: u64,
    pub new_lines: u64,
    pub lines: Vec<DiffLine>,
}

/// The diff for one file.
#[derive(Debug, Clone, Serialize)]
pub struct FileDiff {
    pub path: String,
    /// `true` when the change is staged (diff was taken from the index).
    pub staged: bool,
    /// Unified-diff hunks; empty when the file has no textual change.
    pub hunks: Vec<Hunk>,
    /// Set when the file is binary or too large to diff as text.
    pub note: Option<String>,
}

/// Locate the `git` binary.
///
/// On Windows the Git-for-Windows `cmd/git.exe` is preferred over the MSYS
/// `git` shim, because the shim's path translation mangles `-z` output.
fn git_exe() -> Option<std::path::PathBuf> {
    for candidate in [
        r"C:\Program Files\Git\cmd\git.exe",
        r"C:\Program Files (x86)\Git\cmd\git.exe",
    ] {
        let p = std::path::Path::new(candidate);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    // Fall back to PATH.
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("git.exe"))
        .find(|p| p.is_file())
}

/// Run `git` in the workspace and return stdout.
fn git(workspace: &Path, args: &[&str]) -> Result<String, HarnessError> {
    let exe = git_exe().ok_or_else(|| {
        HarnessError::Tool("git executable not found; install Git for Windows".into())
    })?;
    let out = Command::new(&exe)
        // `-c` is a git-level option: it must come *before* the subcommand.
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .current_dir(workspace)
        .output()
        .map_err(|e| HarnessError::Tool(format!("failed to run git: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(HarnessError::Tool(if err.is_empty() {
            format!("git exited with {}", out.status)
        } else {
            err
        }));
    }
    // G-02：stdout 带上限后再转字符串。
    let mut stdout = out.stdout;
    if stdout.len() > MAX_GIT_OUTPUT {
        stdout.truncate(MAX_GIT_OUTPUT);
    }
    Ok(String::from_utf8_lossy(&stdout).to_string())
}

/// 校验相对路径落在 workspace 内（G-01）：拒绝绝对路径与 `..` 段；词法
/// 规范化后仍须以 workspace 为前缀，防止 `../../etc/...` 越界读文件。
fn check_within(workspace: &Path, path: &str) -> Result<PathBuf, HarnessError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(HarnessError::Tool("path is required".into()));
    }
    // P3：pathspec 魔法串（`:(top)`、`:/foo`、`:!x` …）。git 会把 `:` 开头的
    // 参数当 pathspec 魔法解析，`:/` 直接锚定仓库根——越出 workspace 指向
    // 仓库内任意文件。词法校验挡不住它（`:/x` 拼进 workspace 后仍是合法
    // 相对路径），必须在进 git 之前显式拒绝。
    if trimmed.starts_with(':') {
        return Err(HarnessError::Tool(format!(
            "拒绝 git pathspec 魔法串（以 `:` 开头，可越出工作区锚定仓库根）: {trimmed}"
        )));
    }
    if Path::new(trimmed).is_absolute() {
        return Err(HarnessError::Tool(format!("拒绝绝对路径: {trimmed}")));
    }
    if trimmed.split('/').any(|seg| seg == "..") {
        return Err(HarnessError::Tool(format!("路径含 `..`，已拒绝: {trimmed}")));
    }
    let joined = workspace.join(trimmed);
    let ws_norm = lexical_normalize(workspace);
    let joined_norm = lexical_normalize(&joined);
    if !joined_norm.starts_with(&ws_norm) {
        return Err(HarnessError::Tool(format!("路径越出工作区: {trimmed}")));
    }
    // G-01：词法规范化挡不住工作区内指向外部的符号链接。直读分支会跟随
    // 链接读文件，这里在存在时拒绝链接，并 canonicalize 后复核仍在工作区内。
    if let Ok(md) = std::fs::symlink_metadata(&joined) {
        if md.file_type().is_symlink() {
            return Err(HarnessError::Tool(format!(
                "拒绝跟随符号链接: {trimmed}"
            )));
        }
        if let (Ok(ws_real), Ok(abs_real)) = (workspace.canonicalize(), joined.canonicalize()) {
            if !abs_real.starts_with(&ws_real) {
                return Err(HarnessError::Tool(format!("路径越出工作区: {trimmed}")));
            }
        }
    }
    Ok(joined_norm)
}

/// 不依赖文件系统的词法路径规范化（解析 `.` / `..`）。
fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component::*;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            CurDir => {}
            ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `true` when the workspace is inside a git work tree.
pub fn is_repo(workspace: &Path) -> bool {
    git(workspace, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// Read the working-tree status.
pub fn status(workspace: &Path) -> Result<Status, HarnessError> {
    if !is_repo(workspace) {
        return Ok(Status {
            is_repo: false,
            branch: String::new(),
            files: Vec::new(),
        });
    }

    let branch = git(workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "HEAD".into());

    // `--porcelain=v2 -z` is the machine-readable format: NUL-separated
    // records, each starting with a type tag.
    let raw = git(
        workspace,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    )?;

    let mut files: Vec<FileStatus> = Vec::new();
    for record in raw.split('\0') {
        if record.is_empty() {
            continue;
        }
        // Porcelain v2 record layouts — the path sits at a different index
        // for each record type:
        //   1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
        //   2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>
        //   u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
        //   ? <path>
        //   ! <path>
        let mut parts = record.splitn(2, ' ');
        let tag = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");

        let (xy, path) = match tag {
            "?" | "!" => ("??".to_string(), rest.to_string()),
            "1" => {
                let f: Vec<&str> = rest.split(' ').collect();
                let xy = f.first().copied().unwrap_or("..").to_string();
                // G-03：路径可能含空格，第 7 段之后整体拼回作为路径。
                let path = f.get(7).map(|_| f[7..].join(" ")).unwrap_or_default();
                (xy, path)
            }
            "2" => {
                let f: Vec<&str> = rest.split(' ').collect();
                let xy = f.first().copied().unwrap_or("..").to_string();
                let path = f.get(8).map(|_| f[8..].join(" ")).unwrap_or_default();
                (xy, path)
            }
            "u" => {
                let f: Vec<&str> = rest.split(' ').collect();
                let xy = f.first().copied().unwrap_or("..").to_string();
                let path = f.get(9).map(|_| f[9..].join(" ")).unwrap_or_default();
                (xy, path)
            }
            _ => continue,
        };
        if path.is_empty() {
            continue;
        }

        // X is the index (staged) status, Y the work-tree status.
        let x = xy.chars().next().unwrap_or('.');
        let y = xy.chars().nth(1).unwrap_or('.');
        let staged = x != '.' && x != '?';
        let state = match tag {
            "?" => "untracked".to_string(),
            "!" => "ignored".to_string(),
            _ => {
                let c = if x != '.' { x } else { y };
                match c {
                    'M' => "modified".into(),
                    'A' => "added".into(),
                    'D' => "deleted".into(),
                    'R' => "renamed".into(),
                    'C' => "copied".into(),
                    'T' => "typechange".into(),
                    _ => "modified".into(),
                }
            }
        };
        files.push(FileStatus {
            path: path.replace('\\', "/"),
            code: xy,
            state,
            staged,
        });
    }

    Ok(Status {
        is_repo: true,
        branch,
        files,
    })
}

// ------------------------------------------------------------------ 提交与历史

/// One commit in the history (`recent_commits` 的行)。
#[derive(Debug, Clone, Serialize)]
pub struct CommitInfo {
    /// Full commit hash.
    pub hash: String,
    /// Author name.
    pub author: String,
    /// ISO-8601 author date.
    pub date: String,
    /// First line of the message.
    pub subject: String,
}

/// 新提交的落点信息（`commit_all` 的返回）。
#[derive(Debug, Clone, Serialize)]
pub struct CommitResult {
    /// Short hash of the new commit.
    pub commit: String,
    /// Branch the commit landed on.
    pub branch: String,
    /// The commit subject.
    pub subject: String,
}

/// Current branch name (`HEAD` when detached). Requires a git repository.
pub fn current_branch(workspace: &Path) -> Result<String, HarnessError> {
    if !is_repo(workspace) {
        return Err(HarnessError::Tool("not a git repository".into()));
    }
    Ok(git(workspace, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string())
}

/// `git add -A`（可选）+ `git commit`。
///
/// 消息直接作为 argv 传递、不经 shell，含引号/换行都安全；`-c user.name /
/// user.email` 只在用户没配全局身份时兜底注入，避免无配置环境下提交失败，
/// 也避免把已配置的提交人永久覆盖成内置身份。
pub fn commit_all(
    workspace: &Path,
    message: &str,
    add_all: bool,
) -> Result<CommitResult, HarnessError> {
    let message = message.trim();
    if message.is_empty() {
        return Err(HarnessError::Tool("commit message is required".into()));
    }
    if !is_repo(workspace) {
        return Err(HarnessError::Tool("not a git repository".into()));
    }
    if add_all {
        git(workspace, &["add", "-A"])?;
    }
    let mut args: Vec<String> = Vec::new();
    for (key, fallback) in [("user.name", "Parable"), ("user.email", "parable@local")] {
        let configured = git(workspace, &["config", "--get", key])
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !configured {
            args.push("-c".into());
            args.push(format!("{key}={fallback}"));
        }
    }
    args.push("commit".into());
    args.push("-m".into());
    args.push(message.to_string());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    git(workspace, &refs)?;

    let commit = git(workspace, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();
    let branch = current_branch(workspace)?;
    Ok(CommitResult {
        commit,
        branch,
        subject: message.lines().next().unwrap_or_default().to_string(),
    })
}

/// 最近 `n` 条提交（时间倒序）。字段用单元分隔符 `\x1f` 切开，主题里出现
/// `|` 之类的字符也不会切错。
pub fn recent_commits(workspace: &Path, n: usize) -> Result<Vec<CommitInfo>, HarnessError> {
    if !is_repo(workspace) {
        return Err(HarnessError::Tool("not a git repository".into()));
    }
    let n = n.clamp(1, 100);
    let raw = git(
        workspace,
        &[
            "log",
            "-n",
            &n.to_string(),
            "--pretty=format:%H%x1f%an%x1f%aI%x1f%s",
        ],
    )?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let mut f = line.split('\u{1f}');
        let (Some(hash), Some(author), Some(date), Some(subject)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        if hash.is_empty() {
            continue;
        }
        out.push(CommitInfo {
            hash: hash.to_string(),
            author: author.to_string(),
            date: date.to_string(),
            subject: subject.to_string(),
        });
    }
    Ok(out)
}

// ------------------------------------------------------------------ worktree

/// One entry of `git worktree list`.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreeInfo {
    /// 工作树绝对路径。
    pub path: String,
    /// 检出的分支短名（`refs/heads/` 已剥掉；detached 时为空）。
    pub branch: String,
    /// HEAD 短哈希（前 10 位）。
    pub head: String,
    /// 主工作树（porcelain 列表第一条）。
    pub is_main: bool,
    /// bare 仓库或 detached HEAD。
    pub bare: bool,
    pub detached: bool,
    /// `locked` 原因（未锁为空）。
    pub locked: String,
}

/// worktree 名字白名单（`^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$`）。
///
/// 名字会直接拼进 worktree 路径（`<root>/<name>`），必须保证拼出来的路径
/// 无法穿越：拒绝路径分隔符、`..`、空白与首字符非字母数字的写法。
pub fn valid_worktree_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 1
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_')
}

/// 分支名作为 `-b` 的参数值时的白名单：git 自己会做完整校验，这里只挡
/// 会被当成选项/路径穿越的形态（`-` 开头、`..`、空白与控制字符等）。
fn valid_branch_arg(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= 200
        && !branch.starts_with('-')
        && !branch.starts_with('/')
        && !branch.ends_with('/')
        && !branch.contains("..")
        && !branch.ends_with(".lock")
        && branch != "@"
        && !branch.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// 路径等值比较：统一 `/` 分隔，Windows 下大小写不敏感（NTFS 语义）。
fn paths_equal(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let s = s.replace('\\', "/");
        if cfg!(windows) {
            s.to_ascii_lowercase()
        } else {
            s
        }
    };
    norm(a) == norm(b)
}

/// 解析 `git worktree list --porcelain` 输出。记录间以空行分隔：
///   worktree <path>
///   HEAD <sha>
///   branch refs/heads/<name>   | detached | bare
///   locked [reason]
fn parse_worktree_porcelain(raw: &str) -> Vec<WorktreeInfo> {
    let mut out: Vec<WorktreeInfo> = Vec::new();
    let mut cur: Option<WorktreeInfo> = None;
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if let Some(w) = cur.take() {
                out.push(w);
            }
            continue;
        }
        let (k, v) = line.split_once(' ').unwrap_or((line, ""));
        match k {
            "worktree" => {
                if let Some(w) = cur.take() {
                    out.push(w);
                }
                // porcelain 的第一条永远是主工作树。
                cur = Some(WorktreeInfo {
                    path: v.to_string(),
                    branch: String::new(),
                    head: String::new(),
                    is_main: out.is_empty(),
                    bare: false,
                    detached: false,
                    locked: String::new(),
                });
            }
            "HEAD" => {
                if let Some(w) = cur.as_mut() {
                    w.head = v.chars().take(10).collect();
                }
            }
            "branch" => {
                if let Some(w) = cur.as_mut() {
                    w.branch = v.strip_prefix("refs/heads/").unwrap_or(v).to_string();
                }
            }
            "bare" => {
                if let Some(w) = cur.as_mut() {
                    w.bare = true;
                }
            }
            "detached" => {
                if let Some(w) = cur.as_mut() {
                    w.detached = true;
                }
            }
            "locked" => {
                if let Some(w) = cur.as_mut() {
                    w.locked = v.to_string();
                }
            }
            _ => {}
        }
    }
    if let Some(w) = cur.take() {
        out.push(w);
    }
    out
}

/// worktree 的落点根：仓库工作树根（`rev-parse --show-toplevel`），
/// 取不到时退回 workspace 本身。
fn worktree_root(workspace: &Path) -> PathBuf {
    git(workspace, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.to_path_buf())
}

/// List all worktrees of the workspace's repository.
pub fn worktree_list(workspace: &Path) -> Result<Vec<WorktreeInfo>, HarnessError> {
    if !is_repo(workspace) {
        return Err(HarnessError::Tool("not a git repository".into()));
    }
    let raw = git(workspace, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktree_porcelain(&raw))
}

/// Create a linked worktree at `<repo root>/<name>` with a new branch.
///
/// `branch` 为空时复用 `name` 作为分支名。不带 `--force`：路径已存在时
/// git 自己会拒绝，不会覆盖。
pub fn worktree_add(
    workspace: &Path,
    name: &str,
    branch: &str,
) -> Result<WorktreeInfo, HarnessError> {
    if !is_repo(workspace) {
        return Err(HarnessError::Tool("not a git repository".into()));
    }
    if !valid_worktree_name(name) {
        return Err(HarnessError::Tool(format!(
            "worktree 名字 `{name}` 非法：只允许 1-64 位字母数字、`-`、`_`，且以字母数字开头"
        )));
    }
    let branch = branch.trim();
    let branch = if branch.is_empty() { name } else { branch };
    if !valid_branch_arg(branch) {
        return Err(HarnessError::Tool(format!(
            "分支名 `{branch}` 非法：不能以 `-` 开头、含 `..`、空白或控制字符"
        )));
    }
    let path = worktree_root(workspace).join(name);
    let path_str = path.to_string_lossy().to_string();
    git(
        workspace,
        &["worktree", "add", &path_str, "-b", branch],
    )?;
    Ok(WorktreeInfo {
        path: path_str,
        branch: branch.to_string(),
        head: String::new(),
        is_main: false,
        bare: false,
        detached: false,
        locked: String::new(),
    })
}

/// Remove a linked worktree by name. 主工作树一律拒绝删除。
///
/// 不带 `--force`：工作树里有未提交修改时 git 自己会拒绝——那是保护，
/// 不是缺陷；确要丢弃修改由用户手动 `git worktree remove --force`。
pub fn worktree_remove(workspace: &Path, name: &str) -> Result<(), HarnessError> {
    if !is_repo(workspace) {
        return Err(HarnessError::Tool("not a git repository".into()));
    }
    if !valid_worktree_name(name) {
        return Err(HarnessError::Tool(format!(
            "worktree 名字 `{name}` 非法：只允许 1-64 位字母数字、`-`、`_`，且以字母数字开头"
        )));
    }
    let list = worktree_list(workspace)?;
    let root = worktree_root(workspace);
    let target = root.join(name);
    let target_str = target.to_string_lossy().to_string();
    let Some(hit) = list.iter().find(|w| paths_equal(&w.path, &target_str)) else {
        return Err(HarnessError::Tool(format!(
            "worktree `{name}` 不存在（按 `{}` 匹配）",
            target_str
        )));
    };
    if hit.is_main {
        return Err(HarnessError::Tool(
            "禁止删除主工作树（main worktree）；如需清理请手动操作".into(),
        ));
    }
    git(workspace, &["worktree", "remove", &hit.path])?;
    Ok(())
}

/// Read the diff for one file.
///
/// Staged changes win: if the file has both staged and unstaged edits the
/// unstaged diff is what the user is looking at right now, so it is
/// preferred; the staged diff is used only when there is no unstaged change.
pub fn diff(workspace: &Path, path: &str) -> Result<FileDiff, HarnessError> {
    let path = path.trim().replace('\\', "/");
    if path.is_empty() {
        return Err(HarnessError::Tool("path is required".into()));
    }
    // G-01：路径必须落在工作区内，拒绝绝对路径与 `..` 穿越。
    let abs = check_within(workspace, &path)?;
    if !is_repo(workspace) {
        return Ok(FileDiff {
            path,
            staged: false,
            hunks: Vec::new(),
            note: Some("not a git repository".into()),
        });
    }

    let (staged, raw) = match git(
        workspace,
        &["diff", "--no-color", "--no-ext-diff", "-U3", "--", &path],
    ) {
        Ok(text) if !text.trim().is_empty() => (false, text),
        _ => {
            let text = git(
                workspace,
                &["diff", "--no-color", "--no-ext-diff", "--cached", "-U3", "--", &path],
            )?;
            (true, text)
        }
    };

    if raw.trim().is_empty() {
        // Untracked file: show it as a whole-file addition.
        if abs.is_file() {
            if let Ok(text) = std::fs::read_to_string(&abs) {
                let lines: Vec<DiffLine> = text
                    .lines()
                    .map(|l| DiffLine {
                        sign: '+',
                        text: l.to_string(),
                    })
                    .collect();
                return Ok(FileDiff {
                    path,
                    staged: false,
                    hunks: vec![Hunk {
                        header: "@@ -0,0 +1,@@".into(),
                        old_start: 0,
                        old_lines: 0,
                        new_start: 1,
                        new_lines: lines.len() as u64,
                        lines,
                    }],
                    note: None,
                });
            }
        }
        return Ok(FileDiff {
            path,
            staged,
            hunks: Vec::new(),
            note: Some("no textual changes".into()),
        });
    }

    Ok(FileDiff {
        path,
        staged,
        hunks: parse_unified(&raw),
        note: None,
    })
}

/// Parse a unified diff into hunks.
fn parse_unified(raw: &str) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut cur: Option<Hunk> = None;

    for line in raw.lines() {
        if line.starts_with("@@") {
            if let Some(h) = cur.take() {
                hunks.push(h);
            }
            cur = Some(parse_hunk_header(line));
            continue;
        }
        // Everything before the first hunk is file-level noise (`diff --git`,
        // `index`, `---`, `+++`).
        let Some(h) = cur.as_mut() else { continue };
        let (sign, text) = match line.chars().next() {
            Some('+') => ('+', &line[1..]),
            Some('-') => ('-', &line[1..]),
            Some('\\') => continue, // "\ No newline at end of file"
            _ => (' ', &line[1.min(line.len())..]),
        };
        h.lines.push(DiffLine {
            sign,
            text: text.to_string(),
        });
    }
    if let Some(h) = cur.take() {
        hunks.push(h);
    }
    hunks
}

/// Parse `@@ -a,b +c,d @@ optional section heading`.
fn parse_hunk_header(line: &str) -> Hunk {
    let mut h = Hunk {
        header: line.to_string(),
        old_start: 0,
        old_lines: 0,
        new_start: 0,
        new_lines: 0,
        lines: Vec::new(),
    };
    let nums: Vec<u64> = line
        .split(|c: char| !(c.is_ascii_digit()))
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u64>().ok())
        .collect();
    if nums.len() >= 4 {
        h.old_start = nums[0];
        h.old_lines = nums[1];
        h.new_start = nums[2];
        h.new_lines = nums[3];
    } else if nums.len() >= 2 {
        h.old_start = nums[0];
        h.new_start = nums[1];
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unified_diff_into_hunks() {
        let raw = "diff --git a/a.txt b/a.txt\nindex 111..222 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,4 @@\n one\n-two\n+TWO\n+extra\n three\n";
        let hunks = parse_unified(raw);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_lines, 3);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_lines, 4);
        assert_eq!(h.lines.len(), 5);
        assert_eq!(h.lines[0].sign, ' ');
        assert_eq!(h.lines[0].text, "one");
        assert_eq!(h.lines[1].sign, '-');
        assert_eq!(h.lines[1].text, "two");
        assert_eq!(h.lines[2].sign, '+');
        assert_eq!(h.lines[2].text, "TWO");
    }

    #[test]
    fn parses_hunk_header_without_line_counts() {
        let h = parse_hunk_header("@@ -5 +7 @@");
        assert_eq!(h.old_start, 5);
        assert_eq!(h.new_start, 7);
    }

    #[test]
    fn multiple_hunks_are_split() {
        let raw = "@@ -1,1 +1,1 @@\n a\n@@ -10,1 +10,2 @@\n b\n+c\n";
        let hunks = parse_unified(raw);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[1].old_start, 10);
        assert_eq!(hunks[1].lines.len(), 2);
    }

    #[test]
    fn non_repo_reports_not_a_repo() {
        let dir = std::env::temp_dir().join(format!("parable-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = status(&dir).unwrap();
        assert!(!s.is_repo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P3：pathspec 魔法串必须在进 git 之前被拒——`:/`、`:(top)` 都能锚定
    /// 仓库根越出 workspace，词法校验挡不住。
    #[test]
    fn diff_rejects_pathspec_magic() {
        let dir = std::env::temp_dir().join(format!("parable-git-magic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for p in [":/", ":(top)", ":!foo", ":(icase)README.md", ":/etc/passwd"] {
            let err = diff(&dir, p).unwrap_err();
            assert!(err.to_string().contains("pathspec"), "{p}: {err}");
        }
        // 普通路径不受影响（非 repo 只影响 diff 内容，不影响路径校验报错）。
        assert!(diff(&dir, "a.txt").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_name_whitelist() {
        for ok in ["a", "task-1", "Feature_2", "Z", "x9-y_z"] {
            assert!(valid_worktree_name(ok), "{ok} should pass");
        }
        for bad in [
            "", "-lead", "_lead", ".dot", "a b", "a/b", "a\\b", "..", "a..b", "a:b", "a:b/c",
            "a\nb", "a'b", "中文",
        ] {
            assert!(!valid_worktree_name(bad), "{bad} should be rejected");
        }
        // 65 个字符超长拒绝。
        let long = "a".repeat(65);
        assert!(!valid_worktree_name(&long));
        assert!(valid_worktree_name(&"a".repeat(64)));
    }

    #[test]
    fn branch_arg_whitelist() {
        assert!(valid_branch_arg("feature/x"));
        assert!(valid_branch_arg("task-1"));
        for bad in ["", "-x", "/x", "x/", "a..b", "a b", "a\tb", "@", "p.lock", "a\x01b"] {
            assert!(!valid_branch_arg(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn parses_worktree_porcelain() {
        let raw = "worktree C:/repo/main\nHEAD 0123456789abcdef\nbranch refs/heads/main\n\n\
                   worktree C:/repo/wt1\nHEAD fedcba9876543210\nbranch refs/heads/feature/x\n\n\
                   worktree C:/repo/wt2\nHEAD 1111222233334444\ndetached\nlocked why\n\n\
                   worktree C:/bare.git\nbare\n";
        let wts = parse_worktree_porcelain(raw);
        assert_eq!(wts.len(), 4);
        assert!(wts[0].is_main);
        assert_eq!(wts[0].branch, "main");
        assert_eq!(wts[0].head, "0123456789");
        assert!(!wts[1].is_main);
        assert_eq!(wts[1].branch, "feature/x");
        assert!(wts[2].detached);
        assert!(wts[2].branch.is_empty());
        assert_eq!(wts[2].locked, "why");
        assert!(wts[3].bare);
        assert!(!wts[3].is_main);
    }

    /// worktree 全链路：init → add → list → 重复 add 拒绝 → 主工作树禁删 →
    /// remove。需要真实 git 可执行文件，找不到时跳过。
    #[test]
    fn worktree_add_list_remove_roundtrip() {
        if git_exe().is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("parable-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let id = ["-c", "user.name=t", "-c", "user.email=t@t"];
        git(&dir, &["init", "-q"]).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        let mut args: Vec<&str> = id.to_vec();
        args.extend(["add", "-A"]);
        git(&dir, &args).unwrap();
        let mut cargs: Vec<&str> = id.to_vec();
        cargs.extend(["commit", "-q", "-m", "init"]);
        git(&dir, &cargs).unwrap();

        let wts = worktree_list(&dir).unwrap();
        assert_eq!(wts.len(), 1);
        assert!(wts[0].is_main);

        // add：branch 留空 → 复用 name。
        let wt = worktree_add(&dir, "task-1", "").unwrap();
        assert!(Path::new(&wt.path).is_dir());
        assert_eq!(wt.branch, "task-1");
        let wts = worktree_list(&dir).unwrap();
        assert_eq!(wts.len(), 2);
        assert!(wts.iter().any(|w| !w.is_main && w.branch == "task-1"));

        // 重复 add 同名 → git 拒绝（路径已存在）。
        assert!(worktree_add(&dir, "task-1", "").is_err());
        // 非法名字拒绝。
        assert!(worktree_add(&dir, "../escape", "b").is_err());
        // 主工作树禁删：主工作树目录名恰好也是合法 name 形态。
        let main_name = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if valid_worktree_name(&main_name) {
            assert!(worktree_remove(&dir, &main_name).is_err());
        }
        // 不存在的名字拒绝。
        assert!(worktree_remove(&dir, "no-such-wt").is_err());
        // 正常 remove。
        worktree_remove(&dir, "task-1").unwrap();
        let wts = worktree_list(&dir).unwrap();
        assert_eq!(wts.len(), 1);
        assert!(!Path::new(&wt.path).exists(), "worktree dir should be gone");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
