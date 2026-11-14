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
            let cdp_port: u16 = std::env::var("PARABLE_CDP_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| {
                    std::net::TcpListener::bind(("127.0.0.1", 0))
                        .and_then(|l| l.local_addr())
                        .map(|a| a.port())
                        .unwrap_or(0)
                });
            std::env::set_var("PARABLE_CDP_PORT", cdp_port.to_string());
            let _ = Config::ensure_home();
            // V8 老生代上限：additional_browser_args 的 --js-flags 只作用于
            // 浏览器级进程，渲染进程要靠这个 WebView2 环境变量（必须在任何
            // WebView2 创建之前设置）。界面堆常态 20MB 左右，给 160MB 足够，
            // 超了就 GC 而不是一路吃到 4GB 上限。
            #[cfg(windows)]
            std::env::set_var(
                "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
                "--js-flags=--max-old-space-size=160",
            );
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
                        // 内存：主界面窗口带一组 WebView2 省内存参数。
                        // - renderer-process-limit=1：界面只有一个渲染进程，
                        //   不再为每个导航目标开新渲染器（实测可省几十 MB）
                        // - disable-features：
                        //     CalculateNativeWinOcclusion  遮挡计算在无边框窗口
                        //                                  上反复重排
                        //     BackForwardCache             不再为后退历史常驻
                        //                                 整页快照（几十 MB）
                        //     MediaRouter / OptimizationHints
                        //                                  用不到的后台服务
                        //     HardwareMediaKeyHandling     不额外挂媒体键监听
                        // - js-flags=--max-old-space-size=192：给 V8 明确的
                        //   老生代上限，更早 GC 而不是一路涨到 4GB
                        // - disable-gpu-shader-disk-cache：省掉着色器磁盘索引的
                        //   常驻映射（界面只有简单 2D 合成，用不上）
                        let ui_args = "--renderer-process-limit=1 \
                            --disable-features=CalculateNativeWinOcclusion,BackForwardCache,MediaRouter,OptimizationHints,HardwareMediaKeyHandling \
                            --js-flags=--max-old-space-size=192 \
                            --disable-gpu-shader-disk-cache \
                            --disable-background-timer-throttling=false";
                        if let Err(e) = WebviewWindowBuilder::new(
                            &handle,
                            "main",
                            WebviewUrl::External(parsed),
                        )
                        .title("Parable")
                        .inner_size(1280.0, 800.0)
                        .min_inner_size(480.0, 400.0)
                        .additional_browser_args(ui_args)
                        .build()
                        {
                            tracing::error!("failed to create window: {e}");
                            handle.exit(1);
                        }

                        // 内置浏览器：独立 webview 环境。Tauri 要求「不同
                        // browser args 必须配不同 data directory」，且必须
                        // 用 builder.data_directory()（写进 --user-data-dir
                        // 参数不被识别，会撞上主界面已建的 WebView2 环境，
                        // 报 HRESULT(0x8007139F)「组或资源状态不正确」）。
                        let bw_data_dir = Config::home_dir().join("parable-browser");
                        let _ = std::fs::create_dir_all(&bw_data_dir);
                        let bw_args_pub = format!(
                            "--remote-debugging-port={cdp_port} \
                             --no-first-run --no-default-browser-check \
                             --renderer-process-limit=2 \
                             --js-flags=--max-old-space-size=256"
                        );
                        std::env::set_var("PARABLE_BW_ARGS", &bw_args_pub);
                        {
                            let sig = Config::home_dir().join("browser-window.request");
                            let _ = std::fs::remove_file(&sig);
                        }
                        let bw_handle = handle.clone();
                        let bw_args_for_thread = bw_args_pub.clone();
                        let sig_path = Config::home_dir().join("browser-window.request");
                        tauri::async_runtime::spawn(async move {
                            let mut built = false;
                            loop {
                                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                                if built {
                                    continue;
                                }
                                if !sig_path.is_file() {
                                    continue;
                                }
                                let _ = std::fs::remove_file(&sig_path);
                                // 已有窗口就直接复用（用户手动关过就重建）。
                                if bw_handle.get_webview_window("browser").is_some() {
                                    built = true;
                                    continue;
                                }
                                // WebView2 的窗口/控制器只能在主线程创建：
                                // 在 tokio 工作线程上建会返回
                                // HRESULT(0x8007139F)「组或资源状态不正确」。
                                // 所以把创建动作投递回主线程，并等它跑完。
                                let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
                                let h = bw_handle.clone();
                                let args = bw_args_for_thread.clone();
                                let dd = bw_data_dir.clone();
                                if bw_handle
                                    .run_on_main_thread(move || {
                                        let bw_url: tauri::Url = "about:blank"
                                            .parse()
                                            .expect("about:blank must parse");
                                        #[cfg(windows)]
                                        let res = WebviewWindowBuilder::new(
                                            &h,
                                            "browser",
                                            WebviewUrl::External(bw_url),
                                        )
                                        .title("Parable Browser")
                                        .inner_size(1280.0, 860.0)
                                        .data_directory(dd)
                                        .additional_browser_args(&args)
                                        .build();
                                        #[cfg(not(windows))]
                                        let res = WebviewWindowBuilder::new(
                                            &h,
                                            "browser",
                                            WebviewUrl::External(bw_url),
                                        )
                                        .title("Parable Browser")
                                        .inner_size(1280.0, 860.0)
                                        .data_directory(dd)
                                        .build();
                                        let ok = match res {
                                            Ok(_) => {
                                                tracing::info!(
                                                    "built-in browser window created on demand"
                                                );
                                                true
                                            }
                                            Err(e) => {
                                                tracing::error!(
                                                    "built-in browser window failed: {e}"
                                                );
                                                false
                                            }
                                        };
                                        let _ = tx.send(ok);
                                    })
                                    .is_ok()
                                {
                                    // 投递成功；等一下结果（主线程上同步创建）。
                                    if let Ok(ok) = rx.await {
                                        built = ok;
                                    }
                                }
                            }
                        });
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
