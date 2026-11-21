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
    // P2 资源上限兜底:调用方(mod.rs)对 max_results clamp 到 2000,这里再加
    // 一层绝对上限——条数 5000 / 结果总字节 2MB。宽泛 pattern(如 `.`)扫大
    // 仓库时,光靠 max_results 仍可能把上下文撑爆;两道线任一触顶即停,
    // 并在结果尾注明 truncated。
    const ABS_MAX_HITS: usize = 5000;
    const ABS_MAX_BYTES: usize = 2 * 1024 * 1024;
    // max_results=0 会让"结果 >= 上限"永远成立,首轮就 cap 且提示荒谬的
    // "capped at 0 results"——归一成默认值。
    let max_results = if max_results == 0 { 200 } else { max_results };
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
    let mut total_bytes = 0usize;
    let mut truncated = false;
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
        // 先计数再读:避免实际读取的文件数超过上限一个
        scanned_files += 1;
        if scanned_files > 20_000 {
            break;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                let rel = path.strip_prefix(root).unwrap_or(path);
                let hit = format!(
                    "{}:{}:{}",
                    rel.to_string_lossy().replace('\\', "/"),
                    i + 1,
                    truncate(line, 220)
                );
                // 绝对上限:条数或总字节任一触顶即停,防宽 pattern 撑爆上下文。
                if hits.len() >= ABS_MAX_HITS
                    || total_bytes + hit.len() > ABS_MAX_BYTES
                    || hits.len() >= max_results
                {
                    truncated = true;
                    break 'walk;
                }
                total_bytes += hit.len();
                hits.push(hit);
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
    if truncated {
        model.push_str(&format!(
            "… [truncated: result limit reached (max_results={max_results}, absolute cap {ABS_MAX_HITS} hits / {ABS_MAX_BYTES} bytes); narrow the pattern or path to continue]\n"
        ));
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

    /// P2 上限：超过 max_results 即截断，并在结果尾注明 truncated。
    #[test]
    fn grep_caps_hits_and_notes_truncation() {
        let dir = std::env::temp_dir().join("parable-test-grep-cap");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = String::new();
        for i in 0..6000 {
            body.push_str(&format!("needle {i}\n"));
        }
        std::fs::write(dir.join("a.txt"), &body).unwrap();
        let out = grep(&dir, "needle", None, false, 200).unwrap();
        assert!(out.for_model.starts_with("200 matches"), "capped at max_results");
        assert!(out.for_model.contains("[truncated"), "must note truncation");
    }

    /// P2 上限：CJK 行截到 220 字符≈660 字节，累计总字节超 2MB 时即使条数
    /// 未到绝对上限也必须停——防宽 pattern 把上下文撑爆。
    #[test]
    fn grep_byte_cap_truncates_wide_matches() {
        let dir = std::env::temp_dir().join("parable-test-grep-bytes");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let line = "针".repeat(220); // 660 bytes
        for f in 0..2 {
            let mut body = String::new();
            for _ in 0..2000 {
                body.push_str(&line);
                body.push('\n');
            }
            // 单文件 1.32MB < 2MB（不会被单文件上限跳过），两文件合计 > 2MB
            std::fs::write(dir.join(format!("f{f}.txt")), body).unwrap();
        }
        let out = grep(&dir, "针", None, false, 5000).unwrap();
        assert!(out.for_model.contains("[truncated"), "byte cap must fire");
    }
}
