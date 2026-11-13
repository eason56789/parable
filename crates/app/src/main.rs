//! Parable desktop shell — a Tauri v2 window over the local harness server.
//!
//! The architecture stays what the spec mandates: this process boots the axum
//! server in-process (`harness::server::serve`, 127.0.0.1 + random port +
//! per-run token) and points the webview at `http://127.0.0.1:<port>/?token=…`.
//! No Tauri IPC is used; the page is the same single-file UI a browser tab
//! would load. Closing the window interrupts any running turn, then the
//! process exits.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use harness::config::Config;
use harness::server::{self, SharedState};
use std::sync::Arc;
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::RwLock;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let app = tauri::Builder::default()
        .setup(|app| {
            let _ = Config::ensure_home();
            let mut cfg = Config::load();
            if cfg.workspace.is_empty() {
                // 安装后的工作目录通常是 Program Files/安装目录，不可写；
                // 桌面端默认给用户一个可写、可见的 workspace。
                let workspace = Config::home_dir().join("workspace");
                let _ = std::fs::create_dir_all(&workspace);
                cfg.workspace = workspace.to_string_lossy().to_string();
            }
            let config = Arc::new(RwLock::new(cfg));
            let token = {
                let t = uuid::Uuid::new_v4().simple().to_string();
                t[..24].to_string()
            };

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Fixed desktop port, not 0: the webview origin must stay stable
                // across launches so the UI's localStorage (theme/settings/
                // toolThink …) survives a restart. 7788 is left to the dev
                // browser server; 7789 is the desktop shell's own origin.
                const DESKTOP_PORT: u16 = 7789;
                match server::serve(token.clone(), config, DESKTOP_PORT).await {
                    Ok((port, state, _server_task)) => {
                        // Stash the state so window-close can stop running turns.
                        handle.manage::<SharedState>(state);
                        let url = format!("http://127.0.0.1:{port}/?token={token}");
                        let parsed: tauri::Url =
                            url.parse().expect("127.0.0.1 url must always parse");
                        if let Err(e) = WebviewWindowBuilder::new(
                            &handle,
                            "main",
                            WebviewUrl::External(parsed),
                        )
                        .title("Parable")
                        .inner_size(1280.0, 800.0)
                        .min_inner_size(480.0, 400.0)
                        .build()
                        {
                            tracing::error!("failed to create window: {e}");
                            handle.exit(1);
                        }
                    }
                    Err(e) => {
                        tracing::error!("harness server failed to start: {e}");
                        handle.exit(1);
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build tauri application");

    app.run(|app_handle, event| {
        if let RunEvent::Exit = event {
            // Spec §4: closing the shell aborts running agents before teardown.
            if let Some(state) = app_handle.try_state::<SharedState>() {
                let state = Arc::clone(&*state);
                let _ = app_handle.run_on_main_thread(move || {
                    tauri::async_runtime::block_on(async move {
                        for s in state.store.list("").await {
                            if state.agent.is_running(&s.id).await {
                                state.agent.interrupt(&s.id).await;
                            }
                        }
                    });
                });
            }
        }
    });
}
