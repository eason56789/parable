//! `glob` and `grep`, both `.gitignore`-aware via the `ignore` crate.

use super::*;
use std::path::Path;

pub fn glob(root: &Path, pattern: &str) -> Result<ToolOutcome, HarnessError> {
    use globset::Glob;
    let glob = Glob::new(pattern)
        .map_err(|e| HarnessError::Tool(format!("invalid glob `{pattern}`: {e}")))?;
    let matcher = glob.compile_matcher();

    let mut hits: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    for entry in ignore::WalkBuilder::new(root).hidden(false).build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        scanned += 1;
        if scanned > 60_000 {
            break;
        }
        let rel = path.strip_prefix(root).unwrap_or(path);
        let rel_s = rel.to_string_lossy().replace('\\', "/");
        if matcher.is_match(&rel_s) || matcher.is_match(path) {
            hits.push(rel_s);
        }
        if hits.len() >= 500 {
            break;
        }
    }
    hits.sort();

    let mut model = format!("{} matches for `{pattern}`:\n", hits.len());
    for h in hits.iter().take(500) {
        model.push_str(&format!("  {h}\n"));
    }
    if hits.len() > 500 {
        model.push_str("… (capped at 500)\n");
    }
    if hits.is_empty() {
        model = format!("No files matched `{pattern}` under {}", root.display());
    }

    Ok(ToolOutcome {
        ok: true,
        title: format!("Glob `{pattern}` · {} matches", hits.len()),
        display: DisplayKind::Plain(model.clone()),
        for_model: model,
    })
}

pub fn grep(
    root: &Path,
    pattern: &str,
    glob_filter: Option<&str>,
    ignore_case: bool,
    max_results: usize,
) -> Result<ToolOutcome, HarnessError> {
    use regex::RegexBuilder;
    let mut builder = RegexBuilder::new(pattern);
    builder.case_insensitive(ignore_case);
    let re = builder
        .build()
        .map_err(|e| HarnessError::Tool(format!("invalid regular expression `{pattern}`: {e}")))?;

    let gm = match glob_filter {
        Some(g) if !g.is_empty() => Some(
            globset::Glob::new(g)
                .map_err(|e| HarnessError::Tool(format!("invalid glob `{g}`: {e}")))?
                .compile_matcher(),
        ),
        _ => None,
    };

    let mut hits: Vec<String> = Vec::new();
    let mut scanned_files = 0usize;
    'walk: for entry in ignore::WalkBuilder::new(root).hidden(false).build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(g) = &gm {
            let rel = path.strip_prefix(root).unwrap_or(path);
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            if !g.is_match(&rel_s) && !g.is_match(path) {
                continue;
            }
        }
        // Skip obvious binaries quickly.
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > 2 * 1024 * 1024 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        scanned_files += 1;
        if scanned_files > 20_000 {
            break;
        }
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                let rel = path.strip_prefix(root).unwrap_or(path);
                hits.push(format!("{}:{}:{}", rel.to_string_lossy().replace('\\', "/"), i + 1, truncate(line, 220)));
                if hits.len() >= max_results {
                    break 'walk;
                }
            }
        }
    }

    let mut model = if hits.is_empty() {
        format!("No matches for `{pattern}` under {}", root.display())
    } else {
        format!("{} matches for `{pattern}`:\n", hits.len())
    };
    for h in hits.iter() {
        model.push_str("  ");
        model.push_str(h);
        model.push('\n');
    }
    if hits.len() >= max_results {
        model.push_str(&format!("… (capped at {max_results} results)\n"));
    }

    Ok(ToolOutcome {
        ok: true,
        title: format!("Grep `{pattern}` · {} matches", hits.len()),
        display: DisplayKind::Plain(model.clone()),
        for_model: model,
    })
}

fn truncate(s: &str, n: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= n {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_finds_lines() {
        let dir = std::env::temp_dir().join("parable-test-grep");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello world\nsecond line\n").unwrap();
        let out = grep(&dir, "world", None, false, 10).unwrap();
        assert!(out.for_model.contains("a.txt:1"));
        assert!(out.ok);
    }
}
