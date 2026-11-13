//! Filesystem tools: read, write, edit, list. Path resolution is the security
//! boundary — everything funnels through [`resolve_within`].

use super::*;
use std::path::{Path, PathBuf};

const MAX_READ_LINES: u64 = 20000;
const MAX_LINE_BYTES: usize = 4000;

/// Turn a caller-supplied path into an absolute path inside the workspace.
/// Rejects `..` escapes and symlink escapes.
pub fn resolve_within(workspace: &Path, raw: &str) -> Result<PathBuf, HarnessError> {
    if raw.is_empty() {
        return Err(HarnessError::Tool("path must not be empty".into()));
    }
    let raw = raw.trim();
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        workspace.join(raw)
    };

    // Normalize without requiring the file to exist.
    let normalized = normalize(&candidate);

    // Reject anything that climbs out of the workspace via `..`.
    let ws_norm = normalize(&workspace.to_path_buf());
    if !normalized.starts_with(&ws_norm) {
        // Allow absolute paths outside the workspace only if the caller passes
        // them explicitly and the permission layer has been consulted; still we
        // flag the escape here so the agent cannot silently wander the disk.
        return Err(HarnessError::Tool(format!(
            "path {raw} resolves outside the workspace ({}); use a path inside {} ",
            normalized.display(),
            ws_norm.display()
        )));
    }

    // 从完整路径向上找最长已存在前缀,canonicalize 后校验仍在工作区内。
    // 只检查最终路径会漏掉中间目录里的 symlink/junction——最终分量还不存在
    // 时 canonicalize 会失败,逃逸就藏在这个盲区里。逐级上探(用
    // symlink_metadata,不跟随链接)能拦住任何已存在中间分量的链接逃逸,
    // 包括目标不可达的悬空链接。
    let ws_real = workspace.canonicalize().ok();
    if ws_real.is_some() {
        let mut probe = normalized.clone();
        loop {
            match std::fs::symlink_metadata(&probe) {
                Ok(_) => {
                    match probe.canonicalize() {
                        Ok(real) => {
                            if let Some(ws_r) = &ws_real {
                                if !real.starts_with(ws_r) {
                                    return Err(HarnessError::Tool(format!(
                                        "path {raw} escapes the workspace through a symlink or junction"
                                    )));
                                }
                            }
                        }
                        Err(_) => {
                            // 已存在分量是链接且目标不可达:无法证明它在工作区内,拒绝
                            return Err(HarnessError::Tool(format!(
                                "path {raw} contains an unresolvable symlink escaping the workspace"
                            )));
                        }
                    }
                    break; // 最长已存在前缀检查完毕
                }
                Err(_) => {
                    if !probe.pop() {
                        break;
                    }
                }
            }
        }
    }
    Ok(normalized)
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        use std::path::Component::*;
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

/// Path resolution for the read-only tools (`read_file` / `list_dir` / `glob` /
/// `grep`). The permission layer already whitelists those tools in every mode,
/// so an explicitly absolute path may point outside the workspace — reading a
/// config file or a log elsewhere on the disk is the point. Relative paths
/// still anchor to the workspace and keep every containment check, and `..`
/// segments are normalized away either way. Write tools must keep using
/// [`resolve_within`].
pub fn resolve_readonly(workspace: &Path, raw: &str) -> Result<PathBuf, HarnessError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(HarnessError::Tool("path must not be empty".into()));
    }
    if Path::new(raw).is_absolute() {
        Ok(normalize(&PathBuf::from(raw)))
    } else {
        resolve_within(workspace, raw)
    }
}

fn rel_display(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn read_file(path: &Path, offset: u64, limit: u64) -> Result<ToolOutcome, HarnessError> {
    let meta = std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            HarnessError::NotFound(path.display().to_string())
        } else {
            HarnessError::Io(e)
        }
    })?;
    if meta.is_dir() {
        return Err(HarnessError::Tool(format!(
            "{} is a directory; use list_dir",
            path.display()
        )));
    }
    if meta.len() > 8 * 1024 * 1024 {
        return Err(HarnessError::Tool(format!(
            "{} is {} bytes; too large to read whole",
            path.display(),
            meta.len()
        )));
    }
    // 先读字节嗅探二进制,再解码:直接 read_to_string 会把两种失败混在
    // 一起,模型分不清该换工具还是该转码。
    let bytes = std::fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            HarnessError::NotFound(path.display().to_string())
        } else {
            HarnessError::Io(e)
        }
    })?;
    if looks_binary(&bytes) {
        return Err(HarnessError::Tool(format!(
            "{} looks like a binary file ({} bytes); use `bash` (file/hexdump/strings) instead of read_file",
            path.display(),
            bytes.len()
        )));
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        HarnessError::Tool(format!(
            "cannot decode {} as UTF-8; try `bash` with `iconv -f <encoding> -t utf-8 {{file}} | head`",
            path.display()
        ))
    })?;

    let all: Vec<&str> = text.lines().collect();
    let total = all.len() as u64;
    let start = (offset.saturating_sub(1)) as usize;
    let end = (start + limit.min(MAX_READ_LINES) as usize).min(all.len());
    let lines: Vec<(u64, String)> = (start..end)
        .map(|i| {
            let mut s = all[i].to_string();
            if s.len() > MAX_LINE_BYTES {
                // String::truncate 只能在字符边界上切,直接给字节数会 panic;
                // 回退到最近的边界,极端情况按 lossy 处理。
                let mut cut = MAX_LINE_BYTES;
                while cut > 0 && !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                if cut == 0 {
                    s = String::from_utf8_lossy(&s.as_bytes()[..MAX_LINE_BYTES.min(s.len())])
                        .into_owned();
                } else {
                    s.truncate(cut);
                }
                s.push('…');
            }
            ((i + 1) as u64, s)
        })
        .collect();

    let shown = lines.len();
    let mut model = format!(
        "{} — showing lines {}-{} of {total}\n",
        path.display(),
        start + 1,
        end
    );
    for (n, l) in &lines {
        model.push_str(&format!("{n:>6}\t{l}\n"));
    }
    if end < total as usize {
        model.push_str(&format!(
            "\n… {remaining} more lines. Re-read with offset={next} to continue.",
            remaining = total as usize - end,
            next = end + 1
        ));
    }

    Ok(ToolOutcome {
        ok: true,
        title: format!("Read {} · {total} lines", path.file_name().and_then(|f| f.to_str()).unwrap_or("?")),
        display: DisplayKind::FileRead {
            path: rel_display(Path::new(""), path),
            lines,
            total,
        },
        for_model: model,
    })
    .map(|mut o| {
        if shown == 0 {
            // 区分"文件确实为空"与"请求区间为空":后者给出行数与合法
            // offset 范围,避免模型把翻过头当成空文件。
            o.for_model = if total == 0 {
                format!("{} is empty", path.display())
            } else {
                format!(
                    "{} has {total} lines; the requested range is empty — use offset in 1..={total}",
                    path.display()
                )
            };
        }
        o
    })
}

/// 简单二进制嗅探:头部样本里出现 NUL 或控制字节(排除常见空白)占比过高
/// 即判为二进制,避免把 .png/.dll 当文本硬读。
fn looks_binary(bytes: &[u8]) -> bool {
    const SAMPLE: usize = 8192;
    let sample = &bytes[..bytes.len().min(SAMPLE)];
    if sample.contains(&0u8) {
        return true;
    }
    if sample.is_empty() {
        return false;
    }
    let ctrl = sample
        .iter()
        .filter(|&&b| b < 9 || (14..=31).contains(&b) || b == 127)
        .count();
    // 允许 \t \n \v \f \r(9-13);控制字节超过 10% 基本不是文本
    ctrl * 10 > sample.len()
}

pub fn write_file(path: &Path, content: &str) -> Result<ToolOutcome, HarnessError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existed = path.exists();
    let old = std::fs::read_to_string(path).ok().unwrap_or_default();
    std::fs::write(path, content)?;
    let (added, removed, lines) = diff_lines(&old, content);
    Ok(ToolOutcome {
        ok: true,
        title: format!(
            "{} {} · +{added} -{removed}",
            if existed { "Wrote" } else { "Created" },
            path.file_name().and_then(|f| f.to_str()).unwrap_or("?"),
        ),
        display: DisplayKind::FileDiff {
            path: rel_display(Path::new(""), path),
            lines,
            added,
            removed,
        },
        for_model: format!(
            "{} {} ({} bytes)",
            if existed { "Overwrote" } else { "Created" },
            path.display(),
            content.len()
        ),
    })
}

pub fn edit_file(
    path: &Path,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<ToolOutcome, HarnessError> {
    // 防御:空 old_string 在 matches("") 里处处命中,等于全文替换。
    // mod.rs 已在入口拦截,这里兜底。
    if old.is_empty() {
        return Err(HarnessError::Tool(
            "`old_string` must not be empty; use write_file to replace a whole file".into(),
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|_| {
        HarnessError::NotFound(format!("{} (cannot read it to edit)", path.display()))
    })?;
    let occurrences = text.matches(old).count();
    if occurrences == 0 {
        return Err(HarnessError::Tool(format!(
            "old_string was not found in {} — the file may have changed; read it again",
            path.display()
        )));
    }
    if occurrences > 1 && !replace_all {
        return Err(HarnessError::Tool(format!(
            "old_string appears {occurrences} times in {}; make it unique or set replace_all=true",
            path.display()
        )));
    }
    let updated = if replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    std::fs::write(path, &updated)?;
    let (added, removed, lines) = diff_lines(&text, &updated);
    Ok(ToolOutcome {
        ok: true,
        title: format!(
            "Edited {} · +{added} -{removed}",
            path.file_name().and_then(|f| f.to_str()).unwrap_or("?")
        ),
        display: DisplayKind::FileDiff {
            path: rel_display(Path::new(""), path),
            lines,
            added,
            removed,
        },
        for_model: format!(
            "Edited {} (+{added} -{removed})",
            path.display(),
        ),
    })
}

pub fn list_dir(path: &Path) -> Result<ToolOutcome, HarnessError> {
    const MAX_ENTRIES: usize = 500;
    let mut entries: Vec<(String, u64, bool)> = Vec::new();
    for e in std::fs::read_dir(path)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let meta = e.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        entries.push((name, size, is_dir));
    }
    entries.sort_by(|a, b| (a.2 != b.2, &a.0).cmp(&(b.2 != b.2, &b.0)));

    // 无上限目录(node_modules 之类)一次性倾倒会撑爆上下文,只展示前
    // MAX_ENTRIES 条,超出部分提示换工具。
    let total = entries.len();
    let mut model = format!("Directory {} ({} entries):\n", path.display(), total);
    let mut lines = Vec::new();
    for (name, size, is_dir) in entries.iter().take(MAX_ENTRIES) {
        let line = if *is_dir {
            format!("{name}/")
        } else {
            format!("{name}  ({size} B)")
        };
        model.push_str(&format!("  {line}\n"));
        lines.push((lines.len() as u64 + 1, line));
    }
    if total > MAX_ENTRIES {
        model.push_str(&format!(
            "… and {} more (use glob or narrow the path)\n",
            total - MAX_ENTRIES
        ));
    }

    Ok(ToolOutcome {
        ok: true,
        title: format!("Listed {} · {} entries", path.display(), entries.len()),
        display: DisplayKind::Plain(model.clone()),
        for_model: model,
    })
}

// ------------------------------------------------------------------ diff

pub fn diff_lines(old: &str, new: &str) -> (usize, usize, Vec<DiffLine>) {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut out = Vec::new();
    // 两侧行号各自独立游标:Delete 只推进旧文件,Insert 只推进新文件,
    // Equal 两侧都推进。此前用全局序号当行号,改动后全错位。
    let mut old_no = 0u64;
    let mut new_no = 0u64;
    for change in diff.iter_all_changes() {
        let (sign, o, n) = match change.tag() {
            ChangeTag::Delete => {
                removed += 1;
                old_no += 1;
                ('-', Some(old_no), None)
            }
            ChangeTag::Insert => {
                added += 1;
                new_no += 1;
                ('+', None, Some(new_no))
            }
            ChangeTag::Equal => {
                old_no += 1;
                new_no += 1;
                (' ', Some(old_no), Some(new_no))
            }
        };
        let mut text = change.value().to_string();
        if text.ends_with('\n') {
            text.pop();
        }
        out.push(DiffLine {
            sign,
            old_no: o,
            new_no: n,
            text,
        });
    }
    // Collapse long runs of unchanged lines to keep the card readable.
    (added, removed, compact_diff(out))
}

fn compact_diff(lines: Vec<DiffLine>) -> Vec<DiffLine> {
    const KEEP: usize = 3;
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].sign == ' ' {
            let start = i;
            while i < lines.len() && lines[i].sign == ' ' {
                i += 1;
            }
            let run = &lines[start..i];
            if run.len() > KEEP * 2 {
                out.extend_from_slice(&run[..KEEP]);
                out.push(DiffLine {
                    sign: ' ',
                    old_no: None,
                    new_no: None,
                    text: format!("… {} unchanged lines …", run.len() - KEEP * 2),
                });
                out.extend_from_slice(&run[run.len() - KEEP..]);
            } else {
                out.extend_from_slice(run);
            }
        } else {
            out.push(lines[i].clone());
            i += 1;
        }
    }
    out
}

/// Human-readable diff preview used in the permission panel.
pub fn preview_diff(old: &str, new: &str, max_lines: usize) -> String {
    let (_, _, lines) = diff_lines(old, new);
    lines
        .iter()
        .filter(|l| l.sign != ' ')
        .take(max_lines)
        .map(|l| format!("{}{}", l.sign, l.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Compute the diff an edit *would* make, without writing anything.
pub fn preview_replace(
    path: &Path,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<String, HarnessError> {
    // 与 edit_file 相同的空串防御
    if old.is_empty() {
        return Err(HarnessError::Tool(
            "`old_string` must not be empty; use write_file to replace a whole file".into(),
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|_| {
        HarnessError::NotFound(format!("{} (cannot read it to preview the edit)", path.display()))
    })?;
    let occurrences = text.matches(old).count();
    if occurrences == 0 {
        return Err(HarnessError::Tool(format!(
            "old_string was not found in {}",
            path.display()
        )));
    }
    if occurrences > 1 && !replace_all {
        return Err(HarnessError::Tool(format!(
            "old_string appears {occurrences} times in {}; make it unique or set replace_all=true",
            path.display()
        )));
    }
    let updated = if replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    Ok(preview_diff(&text, &updated, 200))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_escape() {
        let ws = std::env::temp_dir().join("parable-test-ws");
        std::fs::create_dir_all(&ws).unwrap();
        assert!(resolve_within(&ws, "../outside.txt").is_err());
        assert!(resolve_within(&ws, "ok.txt").is_ok());
        assert!(resolve_within(&ws, "a/../b.txt").is_ok());
    }

    #[test]
    fn blocks_symlink_escape() {
        let ws = std::env::temp_dir().join("parable-test-symlink-ws");
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        let outside = std::env::temp_dir().join("parable-test-symlink-outside");
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        // 正常子目录仍然放行
        std::fs::create_dir_all(ws.join("real")).unwrap();
        assert!(resolve_within(&ws, "real/ok.txt").is_ok());
        // Windows 上构造 symlink/junction 需要特权或外部命令,断言只在
        // Unix 下运行(方向一致:工作区内 symlink 指向外部目录必须被拦)。
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, ws.join("link")).unwrap();
            // 最终分量不存在,逃逸藏在中间 symlink 里
            assert!(resolve_within(&ws, "link/escape.txt").is_err());
            // 链接本身也要拦
            assert!(resolve_within(&ws, "link").is_err());
        }
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn readonly_resolution_allows_absolute_outside() {
        let ws = std::env::temp_dir().join("parable-ws-ro");
        // An explicit absolute path elsewhere on the disk is fine for reads.
        let outside = std::env::temp_dir().join("elsewhere").join("f.txt");
        assert_eq!(
            resolve_readonly(&ws, &outside.to_string_lossy()).unwrap(),
            outside
        );
        // `..` segments are still flattened rather than trusted literally.
        assert_eq!(
            resolve_readonly(&ws, &format!("{}\\..\\elsewhere\\f.txt", ws.display())).unwrap(),
            outside
        );
        // Relative paths stay anchored: a `..` climb still lands inside the
        // temp parent here, but the workspace containment check still applies
        // through resolve_within for anything that escapes.
        assert!(resolve_readonly(&ws, "../deep-outside.txt").is_err());
        assert!(resolve_readonly(&ws, "").is_err());
    }

    #[test]
    fn edit_requires_unique_match() {
        let dir = std::env::temp_dir().join("parable-test-edit");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.txt");
        std::fs::write(&f, "aa bb aa").unwrap();
        assert!(edit_file(&f, "aa", "cc", false).is_err());
        assert!(edit_file(&f, "aa", "cc", true).is_ok());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "cc bb cc");
    }

    #[test]
    fn diff_counts() {
        let (a, r, _) = diff_lines("a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!(a, 2);
        assert_eq!(r, 1);
    }

    #[test]
    fn diff_line_numbers_track_both_sides() {
        let (_, _, lines) = diff_lines("a\nb\nc\n", "a\nB\nc\nd\n");
        // 期望语义:上下文 a 在旧1新1、c 在旧3新3;-b 在旧2;+B 在新2、+d 在新4。
        // 组内 Delete/Insert 的相对顺序是 similar 的实现细节,断言只看行号语义。
        let ctx: Vec<(u64, u64)> = lines
            .iter()
            .filter(|l| l.sign == ' ')
            .filter_map(|l| Some((l.old_no?, l.new_no?)))
            .collect();
        assert_eq!(ctx, vec![(1, 1), (3, 3)]);
        // 删除行只带旧行号,新增行只带新行号
        let del: Vec<u64> = lines.iter().filter(|l| l.sign == '-').filter_map(|l| l.old_no).collect();
        assert_eq!(del, vec![2]);
        let ins: Vec<u64> = lines.iter().filter(|l| l.sign == '+').filter_map(|l| l.new_no).collect();
        assert_eq!(ins, vec![2, 4]);
    }

    #[test]
    fn detects_binary_samples() {
        assert!(looks_binary(b"PNG\x00\x00\x01\x00"));
        assert!(!looks_binary(b"plain text with\nnewlines\tand tabs"));
        // 高比例控制字节也拦
        let many_ctrl: Vec<u8> = (0u8..=255).filter(|b| *b < 9 || (14u8..=31).contains(b)).collect();
        assert!(looks_binary(&many_ctrl));
        assert!(!looks_binary(b""));
    }
}
