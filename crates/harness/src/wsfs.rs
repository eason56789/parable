//! Workspace file browsing for the side pane.
//!
//! The `tools/fs.rs` helpers are shaped for the *model*: they return a
//! `ToolOutcome` whose `display` is a human/model readable blob. The side
//! pane needs structured data instead, so this module walks the workspace
//! itself and returns plain DTOs.
//!
//! ## Containment
//!
//! Every path that reaches the filesystem goes through
//! [`crate::tools::fs::resolve_within`], which rejects `..` escapes and
//! symlink/junction escapes. The read-only variant (`resolve_readonly`)
//! deliberately lets absolute paths through — that is right for the model
//! (reading a log outside the workspace is the point) but wrong for a UI
//! browser, so we do not use it here. The `~/.parable` internals are no
//! longer refused outright (the user may browse sessions/logs/config), but
//! secret/key material (`.env*`, `*.key`, `*.pem`, SSH private keys, npm/netrc/
//! git 凭据文件、KeePass/Java 密钥库) is blacklisted from listing, search and
//! read: the HTTP layer bypasses the tool permission gate, so this check is
//! the only thing standing between a caller and the API keys.

use crate::error::HarnessError;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// One entry in a directory listing.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// Path relative to the workspace, using `/` separators. Empty for the
    /// workspace root itself.
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    /// Size in bytes; always 0 for directories.
    pub size: u64,
}

/// A file's contents plus the metadata the UI needs to render it.
#[derive(Debug, Clone, Serialize)]
pub struct FileContent {
    pub path: String,
    pub name: String,
    pub size: u64,
    /// `true` when the bytes are not valid UTF-8 (binary file).
    pub binary: bool,
    /// Decoded text; empty for binary files.
    pub text: String,
    /// 1-based line number of the first returned line.
    pub offset: u64,
    /// Total number of lines in the file.
    pub total_lines: u64,
    /// `true` when more lines follow after `offset + limit`.
    pub truncated: bool,
}

/// A filename search hit.
#[derive(Debug, Clone, Serialize)]
pub struct Match {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
}

/// Maximum entries returned for one directory listing.
const MAX_ENTRIES: usize = 500;

/// Maximum results returned by a filename search.
const MAX_MATCHES: usize = 200;

/// How deep the filename search walks.
const MAX_DEPTH: usize = 12;

/// Directories that are never worth walking or showing.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__"];

/// Files above this size are listed but never opened.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// B-04：密钥/凭据文件名黑名单——列表面板不展示、读取接口拒绝打开。
/// `tree` / `read` / `search` 三条路径都会先过这里，UI 面板没有任何绕过口。
fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // `.env` 前缀覆盖 .env.local / .env.production / .env.staging 等（.env 后
    // 面常跟环境后缀，精确匹配会漏）。
    lower.starts_with(".env")
        // 包管理器 / FTP / git 的凭据文件：npm token、netrc 密码、git 凭据。
        || lower == ".npmrc"
        || lower == "_netrc"
        || lower == ".netrc"
        || lower == ".git-credentials"
        // SSH 私钥
        || lower.starts_with("id_rsa")
        || lower.starts_with("id_ed25519")
        || lower.starts_with("id_ecdsa")
        || lower.starts_with("id_dsa")
        // 证书/私钥容器
        || lower.ends_with(".key")
        || lower.ends_with(".pem")
        || lower.ends_with(".p12")
        || lower.ends_with(".pfx")
        // 密码库 / Java 密钥库
        || lower.ends_with(".kdbx")
        || lower.ends_with(".keystore")
        || lower.ends_with(".jks")
}

/// Resolve a caller-supplied path against the workspace, refusing anything
/// that escapes it. `~/.parable` is no longer special-cased — the user asked
/// to be able to browse sessions/logs/config through the same tools.
///
/// `raw` is workspace-relative; the empty string means the workspace root.
pub fn resolve(workspace: &Path, raw: &str) -> Result<PathBuf, HarnessError> {
    let raw = raw.trim().replace('\\', "/");
    // `resolve_within` rejects the empty string, so map the root to ".".
    let probe = if raw.is_empty() || raw == "." { "." } else { raw.as_str() };
    // 只做工作区越界检查；~/.parable 内部不再特殊拦截（用户要能直接浏览
    // 自己的会话与日志文件）。
    crate::tools::fs::resolve_within(workspace, probe)
}

/// Path relative to the workspace, with `/` separators.
fn rel(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// List one directory level.
///
/// The listing is deliberately one level deep: the tree is expanded lazily in
/// the UI, so a workspace with 50k files costs one `read_dir` per click
/// instead of a full walk.
pub fn tree(workspace: &Path, raw: &str) -> Result<Vec<Entry>, HarnessError> {
    let dir = resolve(workspace, raw)?;
    let meta = std::fs::metadata(&dir).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            HarnessError::NotFound(dir.display().to_string())
        } else {
            HarnessError::Io(e)
        }
    })?;
    if !meta.is_dir() {
        return Err(HarnessError::Tool(format!(
            "{} is not a directory",
            dir.display()
        )));
    }

    let mut out: Vec<Entry> = Vec::new();
    for e in std::fs::read_dir(&dir)? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().to_string();
        if is_secret_name(&name) {
            continue;
        }
        if name.starts_with('.') {
            continue;
        }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(Entry {
            path: rel(workspace, &e.path()),
            name,
            is_dir,
            size,
        });
    }
    // Directories first, then case-insensitive by name. The key is
    // `!is_dir` so that directories (false) sort before files (true).
    out.sort_by(|a, b| {
        (!a.is_dir, a.name.to_lowercase(), a.name.clone()).cmp(&(
            !b.is_dir,
            b.name.to_lowercase(),
            b.name.clone(),
        ))
    });
    out.truncate(MAX_ENTRIES);
    Ok(out)
}

/// Read a text file, one window of lines at a time.
pub fn read(workspace: &Path, raw: &str, offset: u64, limit: u64) -> Result<FileContent, HarnessError> {
    let path = resolve(workspace, raw)?;
    let meta = std::fs::metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            HarnessError::NotFound(path.display().to_string())
        } else {
            HarnessError::Io(e)
        }
    })?;
    if meta.is_dir() {
        return Err(HarnessError::Tool(format!(
            "{} is a directory",
            path.display()
        )));
    }
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if is_secret_name(&file_name) {
        return Err(HarnessError::Tool(format!(
            "{} 是密钥/凭据文件，已拒绝在面板中打开",
            path.display()
        )));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(HarnessError::Tool(format!(
            "{} is {} bytes; too large to open",
            path.display(),
            meta.len()
        )));
    }

    let bytes = std::fs::read(&path).map_err(HarnessError::Io)?;
    // B-17：先尝试一次性 UTF-8 解码，避免为判定编码先克隆整份字节。
    let (binary, text) = match String::from_utf8(bytes) {
        Ok(s) => (false, s),
        Err(e) => (true, String::from_utf8_lossy(e.as_bytes()).into_owned()),
    };
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len() as u64;

    let start = offset.saturating_sub(1) as usize;
    let limit = if limit == 0 { 500 } else { limit as usize };
    let slice: Vec<&str> = lines
        .iter()
        .skip(start)
        .take(limit)
        .map(|s| *s)
        .collect();
    let truncated = (start + slice.len()) < lines.len();

    Ok(FileContent {
        path: rel(workspace, &path),
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        size: meta.len(),
        binary,
        text: slice.join("\n"),
        offset: start as u64 + 1,
        total_lines,
        truncated,
    })
}

/// Search for files by name (case-insensitive substring).
///
/// Walks the workspace breadth-first with a depth cap, so a huge tree costs
/// a bounded amount of work. `..parable` internals are skipped entirely.
pub fn search(workspace: &Path, query: &str) -> Result<Vec<Match>, HarnessError> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<Match> = Vec::new();
    let mut queue: Vec<(PathBuf, usize)> = vec![(workspace.to_path_buf(), 0)];

    while let Some((dir, depth)) = queue.pop() {
        if depth > MAX_DEPTH || out.len() >= MAX_MATCHES {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            if out.len() >= MAX_MATCHES {
                break;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if is_secret_name(&name) {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if name.starts_with('.') {
                continue;
            }
            if is_dir && SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let path = e.path();
            if name.to_lowercase().contains(&q) {
                out.push(Match {
                    path: rel(workspace, &path),
                    name,
                    is_dir,
                });
            }
            if is_dir {
                queue.push((path, depth + 1));
            }
        }
    }
    out.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 每次调用都拿到唯一目录。旧模板只有「name + pid」，同一测试二进制内
    /// pid 相同，`resolve_rejects_parent_escape` 与 `resolve_still_rejects_escape`
    /// 都调 `tmp_ws("escape")` 会撞同一目录；helper 开头的 `remove_dir_all` 把
    /// 另一个并行用例刚建好的 `sub` 删掉，写 `sub/b.md` 时 NotFound（flaky）。
    /// 唯一路径后不再需要开头清理，各用例结束自行 `remove_dir_all`。
    fn tmp_ws(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "parable-wsfs-{}-{}-{}",
            name,
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.join("sub/b.md"), "# b").unwrap();
        dir
    }

    #[test]
    fn tree_lists_dirs_first_and_skips_hidden() {
        let ws = tmp_ws("tree");
        std::fs::write(ws.join(".secret"), "x").unwrap();
        let entries = tree(&ws, "").unwrap();
        assert_eq!(entries[0].name, "sub");
        assert!(entries[0].is_dir);
        assert!(entries.iter().all(|e| e.name != ".secret"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn resolve_rejects_parent_escape() {
        let ws = tmp_ws("escape");
        let err = resolve(&ws, "../outside").unwrap_err();
        assert!(
            err.to_string().contains("outside the workspace")
                || err.to_string().contains("escapes"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn resolve_allows_parable_internals() {
        // ~/.parable 现在只是普通目录（用户要能通过同一套工具浏览会话、
        // 日志与配置）。工作区包含它时，解析必须成功而不是被内部守卫拒绝。
        let home = crate::config::Config::home_dir();
        let ws = home.parent().unwrap_or(&home).to_path_buf();
        let rel = Path::new(".parable").join("sessions");
        let ok = resolve(&ws, rel.to_string_lossy().as_ref())
            .expect("~/.parable paths must resolve now");
        assert!(ok.ends_with(Path::new(".parable").join("sessions")));
    }

    #[test]
    fn resolve_still_rejects_escape() {
        // 移除内部守卫不等于放开越界：工作区之外仍然拒绝。
        let ws = tmp_ws("escape");
        assert!(resolve(&ws, "../outside.txt").is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn read_returns_line_window() {
        let ws = tmp_ws("read");
        let f = read(&ws, "a.txt", 2, 1).unwrap();
        assert_eq!(f.text, "two");
        assert_eq!(f.offset, 2);
        assert_eq!(f.total_lines, 3);
        assert!(f.truncated);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn read_blocks_secret_files() {
        let ws = tmp_ws("secret");
        std::fs::write(ws.join(".env"), "KEY=secret").unwrap();
        std::fs::write(ws.join("server.key"), "abc").unwrap();
        assert!(read(&ws, ".env", 1, 10).is_err());
        assert!(read(&ws, "server.key", 1, 10).is_err());
        let entries = tree(&ws, "").unwrap();
        assert!(entries.iter().all(|e| !is_secret_name(&e.name)));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn search_finds_by_name_case_insensitively() {
        let ws = tmp_ws("search");
        let hits = search(&ws, "B.MD").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "sub/b.md");
        let _ = std::fs::remove_dir_all(&ws);
    }
}
