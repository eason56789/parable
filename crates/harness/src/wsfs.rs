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
    /// Total number of lines in the file. **0 for binary files**: counting
    /// newlines in a PNG is meaningless and the UI printed things like
    /// "56 KB · 174 行" under an image.
    pub total_lines: u64,
    /// `true` when more lines follow after `offset + limit`.
    pub truncated: bool,
    /// MIME type when the file is an image (sniffed from magic bytes, not the
    /// extension), so the side pane can render a preview instead of hex noise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

/// Large images are still previewable; this cap only guards the JSON path.
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

/// Image MIME sniffing from magic bytes.
///
/// Extensions lie: a `.png` is sometimes a JPEG, and a `.svg` is text. The UI
/// decides "show a picture" vs "show text" from this, so it must look at the
/// bytes. Returns `None` for anything that is not a recognisable raster image.
pub fn image_mime(name: &str, head: &[u8]) -> Option<&'static str> {
    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    // JPEG: FF D8 FF
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    // GIF87a / GIF89a
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // RIFF....WEBP
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    // BMP: "BM"
    if head.starts_with(b"BM") {
        return Some("image/bmp");
    }
    // ISO-BMFF: ....ftypavif / ....ftypavis
    if head.len() >= 12 && &head[4..8] == b"ftyp" && (&head[8..12] == b"avif" || &head[8..12] == b"avis") {
        return Some("image/avif");
    }
    // ICO: 00 00 01 00
    if head.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        return Some("image/x-icon");
    }
    // SVG is text: only trust it when the file is actually named .svg,
    // otherwise every XML-ish text file would be treated as a picture.
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "svg" {
        let probe = String::from_utf8_lossy(&head[..head.len().min(512)]);
        let probe = probe.trim_start().to_ascii_lowercase();
        if probe.starts_with("<svg") || probe.starts_with("<?xml") || probe.starts_with("<!doctype svg") {
            return Some("image/svg+xml");
        }
    }
    None
}

/// Raw image bytes for the side pane / chat markdown renderer.
///
/// Refuses anything that is not a recognised image so this endpoint can never
/// be used as a generic "read any file, bypassing the text blacklist" hole.
pub fn read_image(workspace: &Path, raw: &str) -> Result<(Vec<u8>, String, String), HarnessError> {
    let path = resolve(workspace, raw)?;
    let meta = std::fs::metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            HarnessError::NotFound(path.display().to_string())
        } else {
            HarnessError::Io(e)
        }
    })?;
    if meta.is_dir() {
        return Err(HarnessError::Tool(format!("{} is a directory", path.display())));
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if is_secret_name(&name) {
        return Err(HarnessError::Tool(format!(
            "{} 是密钥/凭据文件，已拒绝在面板中打开",
            path.display()
        )));
    }
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(HarnessError::Tool(format!(
            "{} is {} bytes; too large to preview",
            path.display(),
            meta.len()
        )));
    }
    let bytes = std::fs::read(&path).map_err(HarnessError::Io)?;
    let Some(mime) = image_mime(&name, &bytes) else {
        return Err(HarnessError::Tool(format!(
            "{} is not a recognised image",
            path.display()
        )));
    };
    Ok((bytes, mime.to_string(), name))
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
    // 图片（按魔数判定，扩展名不算数）：不按行切、不做 lossy 解码——把 PNG
    // 的字节按 UTF-8 读成文本，既浪费内存又会让面板显示一堆乱码，还会把
    // 「换行字节数」当成行数报出来（用户看到 PNG 底下写着「174 行」）。
    let image = image_mime(&file_name, &bytes);
    if let Some(mime) = image {
        return Ok(FileContent {
            path: rel(workspace, &path),
            name: file_name,
            size: meta.len(),
            binary: true,
            text: String::new(),
            offset: 1,
            total_lines: 0,
            truncated: false,
            image: Some(mime.to_string()),
        });
    }
    // B-17：先尝试一次性 UTF-8 解码，避免为判定编码先克隆整份字节。
    let (binary, text) = match String::from_utf8(bytes) {
        Ok(s) => (false, s),
        Err(e) => (true, String::from_utf8_lossy(e.as_bytes()).into_owned()),
    };
    // 二进制文件没有「行」的概念：`lines()` 会把任意 0x0A 字节当成换行，
    // 报出来的行数纯属噪音。只有文本才统计。
    let lines: Vec<&str> = if binary { Vec::new() } else { text.lines().collect() };
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
        image: None,
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
    fn image_mime_sniffs_magic_bytes_not_extension() {
        // 真 PNG 头
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        assert_eq!(image_mime("a.png", &png), Some("image/png"));
        // 扩展名说谎：叫 .png 但字节是 JPEG
        let jpg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 16];
        assert_eq!(image_mime("a.png", &jpg), Some("image/jpeg"));
        // GIF / WEBP / BMP
        assert_eq!(image_mime("x", b"GIF89a......"), Some("image/gif"));
        let mut webp = Vec::from(*b"RIFF\0\0\0\0WEBPVP8 ");
        webp.truncate(12);
        assert_eq!(image_mime("x.webp", &webp), Some("image/webp"));
        assert_eq!(image_mime("x.bmp", b"BM\0\0"), Some("image/bmp"));
        // 文本不是图片
        assert_eq!(image_mime("notes.txt", b"hello world"), None);
        // SVG 只在 .svg 名下、且内容像 SVG 时才算图片
        assert_eq!(image_mime("a.svg", b"<svg xmlns=\"...\">"), Some("image/svg+xml"));
        assert_eq!(image_mime("a.txt", b"<svg>"), None);
    }

    #[test]
    fn read_reports_image_and_no_line_count() {
        let ws = tmp_ws("img");
        // 一个最小合法 PNG（1x1），带若干 0x0A 字节——旧实现会把它们当换行数出来。
        let png: [u8; 33] = [
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
            0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x0A, 0x0A, 0x0A, 0,
        ];
        std::fs::write(ws.join("p.png"), png).unwrap();
        let f = read(&ws, "p.png", 1, 500).unwrap();
        assert_eq!(f.image.as_deref(), Some("image/png"));
        assert!(f.binary);
        assert_eq!(f.text, "", "image bytes must not be decoded as text");
        assert_eq!(f.total_lines, 0, "an image has no line count");
        // 文本文件仍然正常统计
        let t = read(&ws, "a.txt", 1, 500).unwrap();
        assert_eq!(t.image, None);
        assert_eq!(t.total_lines, 3);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn read_image_refuses_non_images_and_escapes() {
        let ws = tmp_ws("raw");
        // 文本文件不能走图片接口
        assert!(read_image(&ws, "a.txt").is_err());
        // 越界路径拒绝
        assert!(read_image(&ws, "../../../Windows/win.ini").is_err());
        // 真图片可以
        std::fs::write(ws.join("ok.gif"), b"GIF89a\x01\x00\x01\x00").unwrap();
        let (bytes, mime, name) = read_image(&ws, "ok.gif").unwrap();
        assert_eq!(mime, "image/gif");
        assert_eq!(name, "ok.gif");
        assert_eq!(bytes.len(), 10);
        let _ = std::fs::remove_dir_all(&ws);
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
