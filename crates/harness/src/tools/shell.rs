//! `bash` tool: run a command, stream its output, enforce a timeout.

use super::*;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

const MAX_OUTPUT: usize = 24_000;

fn shell_pair() -> (String, Vec<String>) {
    // Git Bash / MSYS on Windows keeps `grep`, `ls`, `cargo` etc. usable; cmd.exe
    // does not. Prefer bash when it is on the PATH or in the usual install spots.
    if cfg!(windows) {
        for candidate in [
            "C:/Program Files/Git/bin/bash.exe",
            "C:/Program Files (x86)/Git/bin/bash.exe",
        ] {
            if std::path::Path::new(candidate).exists() {
                return (candidate.to_string(), vec!["-lc".into()]);
            }
        }
        let from_path = std::process::Command::new("where")
            .arg("bash")
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .map(|s| s.trim().to_string())
            })
            .filter(|s| !s.is_empty());
        if let Some(b) = from_path {
            return (b, vec!["-lc".into()]);
        }
        return ("cmd".into(), vec!["/C".into()]);
    }
    ("bash".into(), vec!["-lc".into()])
}

pub async fn bash(
    command: &str,
    cwd: &Path,
    timeout_ms: u64,
    ctx: &ToolCtx,
) -> Result<ToolOutcome, HarnessError> {
    let (prog, args) = shell_pair();
    let mut cmd = tokio::process::Command::new(&prog);
    cmd.args(&args)
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| HarnessError::Tool(format!("failed to spawn {prog}: {e}")))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let hub = ctx.hub.clone();
    let sid = ctx.session_id.clone();
    let cid = ctx.call_id.clone();

    let out_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(out) = stdout {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if buf.len() < MAX_OUTPUT {
                    hub.publish(&sid, "tool.progress", json!({"callId": cid, "chunk": line}));
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
        }
        buf
    });
    let err_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(err) = stderr {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if buf.len() < MAX_OUTPUT {
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
        }
        buf
    });

    let status = tokio::select! {
        st = child.wait() => st.map_err(|e| HarnessError::Tool(format!("wait failed: {e}")))?,
        _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms.max(1000))) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Ok(ToolOutcome {
                ok: false,
                title: format!("Timed out after {}ms", timeout_ms),
                display: DisplayKind::Shell {
                    command: command.to_string(),
                    stdout: out_task.await.unwrap_or_default(),
                    stderr: "process was killed after the timeout".into(),
                    exit: -1,
                },
                for_model: format!("Command timed out after {timeout_ms}ms and was killed:\n{command}"),
            });
        }
    };

    let stdout = out_task.await.unwrap_or_default();
    let stderr = err_task.await.unwrap_or_default();
    let code = status.code().unwrap_or(-1);

    let title = if code == 0 {
        format!("Ran `{}` · exit 0", truncate(command, 48))
    } else {
        format!("Ran `{}` · exit {code}", truncate(command, 48))
    };

    let mut for_model = String::new();
    for_model.push_str(&format!("exit code: {code}\n"));
    if !stdout.trim().is_empty() {
        for_model.push_str("stdout:\n");
        for_model.push_str(&clamp(&stdout, MAX_OUTPUT));
        for_model.push('\n');
    }
    if !stderr.trim().is_empty() {
        for_model.push_str("stderr:\n");
        for_model.push_str(&clamp(&stderr, MAX_OUTPUT));
    }
    if stdout.trim().is_empty() && stderr.trim().is_empty() {
        for_model.push_str("(no output)");
    }

    Ok(ToolOutcome {
        ok: code == 0,
        title,
        display: DisplayKind::Shell {
            command: command.to_string(),
            stdout: clamp(&stdout, MAX_OUTPUT),
            stderr: clamp(&stderr, MAX_OUTPUT),
            exit: code,
        },
        for_model,
    })
}

fn clamp(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…\n[output truncated, {} bytes total]", &s[..end], s.len())
    }
}
