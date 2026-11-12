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

    println!("Parable harness listening on {url}");

    if let Some(prompt) = args.prompt {
        run_headless(&state, &prompt).await?;
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
) -> anyhow::Result<()> {
    use harness::agent::TurnOptions;
    let sid = {
        let cfg = state.config.read().await;
        let ws = cfg.workspace.clone();
        state.store.create(&ws, "headless").await.meta.id
    };
    let turn_id = state
        .agent
        .start_turn(&sid, prompt, vec![], TurnOptions::default())
        .await?;
    tracing::info!("headless turn {turn_id} on session {sid} started");

    // Drain the event hub until the turn ends, echoing text to stdout.
    let mut rx = state.hub.subscribe();
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if ev.session_id != sid {
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
            Err(_) => break,
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
