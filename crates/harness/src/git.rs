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
use std::path::Path;
use std::process::Command;

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
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
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
                let path = f.get(7).copied().unwrap_or("").to_string();
                (xy, path)
            }
            "2" => {
                let f: Vec<&str> = rest.split(' ').collect();
                let xy = f.first().copied().unwrap_or("..").to_string();
                let path = f.get(8).copied().unwrap_or("").to_string();
                (xy, path)
            }
            "u" => {
                let f: Vec<&str> = rest.split(' ').collect();
                let xy = f.first().copied().unwrap_or("..").to_string();
                let path = f.get(9).copied().unwrap_or("").to_string();
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
        let abs = workspace.join(&path);
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
}
