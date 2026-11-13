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
//! browser, so we do not use it here. On top of that, `~/.parable` internals
//! (config, permissions, sessions, uploads) are refused outright: the HTTP
//! layer bypasses the tool permission gate, so this check is the only thing
//! standing between a caller and the API keys.

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

/// Resolve a caller-supplied path against the workspace, refusing anything
/// that escapes it or lands in `~/.parable`.
///
/// `raw` is workspace-relative; the empty string means the workspace root.
pub fn resolve(workspace: &Path, raw: &str) -> Result<PathBuf, HarnessError> {
    let raw = raw.trim().replace('\\', "/");
    // `resolve_within` rejects the empty string, so map the root to ".".
    let probe = if raw.is_empty() || raw == "." { "." } else { raw.as_str() };
    let path = crate::tools::fs::resolve_within(workspace, probe)?;
    if crate::permission::is_parable_internal(&path) {
        return Err(HarnessError::PermissionDenied(
            "refusing to touch ~/.parable internals".into(),
        ));
    }
    Ok(path)
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
        if name.starts_with('.') && name != ".env" {
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
    if meta.len() > MAX_FILE_BYTES {
        return Err(HarnessError::Tool(format!(
            "{} is {} bytes; too large to open",
            path.display(),
            meta.len()
        )));
    }

    let bytes = std::fs::read(&path).map_err(HarnessError::Io)?;
    let binary = String::from_utf8(bytes.clone()).is_err();
    let text = String::from_utf8_lossy(&bytes).to_string();
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
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if name.starts_with('.') && name != ".env" {
                continue;
            }
            if is_dir && SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let path = e.path();
            if crate::permission::is_parable_internal(&path) {
                continue;
            }
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

    fn tmp_ws(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("parable-wsfs-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
    fn resolve_rejects_parable_internals() {
        // Point the workspace at the home directory so that `~/.parable`
        // really is *inside* it — otherwise the containment check fires
        // first and we would not be exercising the internals guard at all.
        let home = crate::config::Config::home_dir();
        let ws = home.parent().unwrap_or(&home).to_path_buf();
        let rel = Path::new(".parable").join("sessions");
        let err = resolve(&ws, rel.to_string_lossy().as_ref()).unwrap_err();
        assert!(err.to_string().contains("~/.parable"), "unexpected: {err}");
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
    fn search_finds_by_name_case_insensitively() {
        let ws = tmp_ws("search");
        let hits = search(&ws, "B.MD").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "sub/b.md");
        let _ = std::fs::remove_dir_all(&ws);
    }
}
