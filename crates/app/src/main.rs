//! Desktop shell. Currently boots the harness server and opens the UI in the
//! system webview/browser; the Tauri window wrapper lands in the next stage.

use harness::config::Config;
use harness::server;
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let _ = Config::ensure_home();
    let mut cfg = Config::load();
    if cfg.workspace.is_empty() {
        cfg.workspace = std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
    }
    let config = Arc::new(RwLock::new(cfg));

    let token = {
        let t = uuid::Uuid::new_v4().simple().to_string();
        t[..24].to_string()
    };
    let (port, _state, _handle) = server::serve(token.clone(), config).await?;
    let url = format!("http://127.0.0.1:{port}/?token={token}");
    println!("Parable UI at {url}");

    open_browser(&url);
    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}
