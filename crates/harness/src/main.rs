//! `parable` — headless entry point. Starts the local harness server and prints
//! the URL the desktop shell (or a browser) should load.

use clap::Parser;
use harness::config::Config;
use harness::server;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Parser, Debug)]
#[command(
    name = "parable",
    about = "Parable AI harness — local agent server with a web UI"
)]
struct Args {
    /// Directory the agent works in. Defaults to the configured workspace.
    #[arg(long)]
    workspace: Option<String>,

    /// Print the URL and exit immediately after binding (used by the desktop shell).
    #[arg(long)]
    port_file: Option<String>,

    /// Bind a fixed port instead of a random one (0 = random). Keeps the
    /// browser tab usable across restarts.
    #[arg(long, default_value_t = 0)]
    port: u16,

    /// Use a fixed auth token instead of a random per-run one. Only pass this
    /// for local development — the default regenerates on every start.
    #[arg(long)]
    token: Option<String>,

    /// Run a single prompt headless and print the result (no UI).
    #[arg(short = 'p', long)]
    prompt: Option<String>,

    /// Headless (-p) overall timeout in seconds, guarding against hangs.
    #[arg(long, default_value_t = 1800)]
    headless_timeout_secs: u64,

    /// Resume an existing session by id. Exits non-zero if it does not exist;
    /// never silently creates a new session.
    #[arg(long = "session-id")]
    session_id: Option<String>,

    /// Repeatable: extra directories the agent may access outside the workspace.
    #[arg(long = "add-dir")]
    add_dir: Vec<String>,

    /// Emit NDJSON events on stdout (`{"kind","session","ts","payload"}`) and
    /// suppress all human-readable decorative output.
    #[arg(long)]
    json: bool,

    /// Permission mode: `ask` | `ws` | `full`. An invalid value exits non-zero.
    #[arg(long = "permission-mode")]
    permission_mode: Option<String>,
}

/// Validate a `--permission-mode` value against the known modes.
fn validate_permission_mode(mode: &str) -> Result<(), String> {
    if harness::config::PERMISSION_MODES.contains(&mode) {
        Ok(())
    } else {
        Err(format!(
            "invalid permission-mode `{mode}`; expected one of: {}",
            harness::config::PERMISSION_MODES.join(", ")
        ))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let _ = Config::ensure_home();

    let mut cfg = Config::load();
    if let Some(ws) = &args.workspace {
        cfg.workspace = ws.clone();
    }
    if let Some(pm) = &args.permission_mode {
        validate_permission_mode(pm)
            .map_err(|e| anyhow::anyhow!(e))?;
        cfg.permission_mode = pm.clone();
    }
    for d in &args.add_dir {
        cfg.extra_dirs.push(d.clone());
    }
    cfg.normalize();
    if cfg.workspace.is_empty() {
        cfg.workspace = std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
    }
    let config = Arc::new(RwLock::new(cfg));

    let token = args.token.clone().filter(|t| !t.is_empty()).unwrap_or_else(generate_token);
    let (port, state, _handle) = server::serve(token.clone(), config.clone(), args.port).await?;
    let url = format!("http://127.0.0.1:{port}/?token={token}");

    if let Some(pf) = &args.port_file {
        let _ = std::fs::write(pf, format!("{port}\n{token}\n"));
    }

    // --json 模式下不打印人类可读的监听横幅，stdout 只留给 NDJSON 事件。
    if !args.json {
        println!("Parable harness listening on {url}");
    }

    if let Some(prompt) = args.prompt {
        run_headless(&state, &prompt, args.headless_timeout_secs, args.session_id, args.json).await?;
        return Ok(());
    }

    // Keep the process alive until Ctrl+C.
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    Ok(())
}

async fn run_headless(
    state: &server::SharedState,
    prompt: &str,
    timeout_secs: u64,
    session_id: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    use harness::agent::TurnOptions;
    let sid = match session_id {
        Some(id) => {
            // 续跑：会话必须已存在；不存在直接报错退出，不静默新建。
            if !state.store.exists(&id).await {
                anyhow::bail!("session `{id}` does not exist; refusing to create a new one");
            }
            id
        }
        None => {
            let cfg = state.config.read().await;
            let ws = cfg.workspace.clone();
            state.store.create(&ws, "headless").await.meta.id
        }
    };
    // 先订阅、后 start_turn：反过来会在 turn 启动瞬间丢掉最早的事件
    // （text.delta 竞态，输出缺开头）。
    let mut rx = state.hub.subscribe();
    let turn_id = state
        .agent
        .start_turn(&sid, prompt, vec![], TurnOptions::default())
        .await?;
    tracing::info!("headless turn {turn_id} on session {sid} started");

    // Drain the event hub until the turn ends, echoing text to stdout.
    // 整体带超时（默认 30 分钟，--headless-timeout-secs 可调），防挂死。
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => {
                if !json {
                    eprintln!("\n[timeout] headless run exceeded {timeout_secs}s; giving up");
                }
                break;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                // 消费端一时跟不上：跳过缺失继续，不能直接退出。
                if !json {
                    eprintln!("\n[warn] event bus lagged, skipped {n} events");
                }
                continue;
            }
            Ok(Err(_)) => break, // channel closed
            Ok(Ok(ev)) => {
                if ev.session_id != sid {
                    continue;
                }
                if json {
                    // NDJSON：一行一个事件对象，payload 原样内嵌；不做人类装饰。
                    let line = serde_json::json!({
                        "kind": ev.kind,
                        "session": ev.session_id,
                        "ts": ev.ts,
                        "payload": ev.payload,
                    });
                    println!("{line}");
                    if ev.kind == "turn.end" {
                        break;
                    }
                    continue;
                }
                match ev.kind.as_str() {
                    "text.delta" => {
                        if let Some(t) = ev.payload.get("text").and_then(|x| x.as_str()) {
                            print!("{t}");
                            use std::io::Write;
                            let _ = std::io::stdout().flush();
                        }
                    }
                    "tool.result" => {
                        eprintln!(
                            "\n[tool] {}",
                            ev.payload.get("title").and_then(|x| x.as_str()).unwrap_or("")
                        );
                    }
                    "error" => {
                        eprintln!(
                            "\n[error] {}",
                            ev.payload.get("message").and_then(|x| x.as_str()).unwrap_or("")
                        );
                    }
                    "turn.end" => {
                        println!();
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn generate_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        nanos.hash(&mut h);
        std::process::id().hash(&mut h);
        format!("{:016x}", h.finish())
    };
    format!("{hash}{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn parse_defaults() {
        let args = Args::try_parse_from(["parable"]).expect("parse defaults");
        assert!(args.session_id.is_none());
        assert!(args.add_dir.is_empty());
        assert!(!args.json);
        assert!(args.permission_mode.is_none());
    }

    #[test]
    fn parse_session_json_mode_and_repeatable_add_dir() {
        let args = Args::try_parse_from([
            "parable",
            "--session-id",
            "sess-123",
            "--json",
            "--permission-mode",
            "ws",
            "--add-dir",
            "C:/data/a",
            "--add-dir",
            "C:/data/b",
        ])
        .expect("parse flags");
        assert_eq!(args.session_id.as_deref(), Some("sess-123"));
        assert!(args.json);
        assert_eq!(args.permission_mode.as_deref(), Some("ws"));
        assert_eq!(args.add_dir, vec!["C:/data/a", "C:/data/b"]);
    }

    #[test]
    fn permission_mode_validation() {
        for m in ["ask", "ws", "full"] {
            assert!(validate_permission_mode(m).is_ok(), "mode {m} should be valid");
        }
        assert!(validate_permission_mode("yolo").is_err());
        assert!(validate_permission_mode("").is_err());
    }

    #[test]
    fn clap_rejects_unknown_flag() {
        assert!(Args::try_parse_from(["parable", "--no-such-flag"]).is_err());
    }
}
