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

    // If the path exists, canonicalize and re-check to defeat symlinks.
    if let Ok(real) = candidate.canonicalize() {
        if let Ok(ws_real) = workspace.canonicalize() {
            if !real.starts_with(&ws_real) {
                return Err(HarnessError::Tool(format!(
                    "path {raw} is a symlink escaping the workspace"
                )));
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
    let text = std::fs::read_to_string(path).map_err(|e| {
        HarnessError::Tool(format!("cannot read {} as UTF-8 text: {e}", path.display()))
    })?;

    let all: Vec<&str> = text.lines().collect();
    let total = all.len() as u64;
    let start = (offset.saturating_sub(1)) as usize;
    let end = (start + limit.min(MAX_READ_LINES) as usize).min(all.len());
    let lines: Vec<(u64, String)> = (start..end)
        .map(|i| {
            let mut s = all[i].to_string();
            if s.len() > MAX_LINE_BYTES {
                s.truncate(MAX_LINE_BYTES);
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
            o.for_model = format!("{} is empty", path.display());
        }
        o
    })
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

    let mut model = format!("Directory {} ({} entries):\n", path.display(), entries.len());
    let mut lines = Vec::new();
    for (name, size, is_dir) in &entries {
        let line = if *is_dir {
            format!("{name}/")
        } else {
            format!("{name}  ({size} B)")
        };
        model.push_str(&format!("  {line}\n"));
        lines.push((lines.len() as u64 + 1, line));
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
    for (idx, change) in diff.iter_all_changes().enumerate() {
        let (sign, old_no, new_no) = match change.tag() {
            ChangeTag::Delete => {
                removed += 1;
                ('-', Some(idx as u64 + 1), None)
            }
            ChangeTag::Insert => {
                added += 1;
                ('+', None, Some(idx as u64 + 1))
            }
            ChangeTag::Equal => (' ', Some(idx as u64 + 1), Some(idx as u64 + 1)),
        };
        let mut text = change.value().to_string();
        if text.ends_with('\n') {
            text.pop();
        }
        out.push(DiffLine {
            sign,
            old_no,
            new_no,
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
}
