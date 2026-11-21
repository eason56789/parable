# Tauri 桌面壳与 Windows 安装包

## 架构

`crates/app` 是一个 Tauri v2 桌面进程：

1. 进程内启动 `harness::server::serve`，监听 `127.0.0.1:7789`（固定桌面端口，避开开发浏览器用的
   7788），并生成每次运行独立的 token；固定端口保证 webview 的 origin 跨启动不变，localStorage
   中的主题/设置（含 `toolThink`）不会每次重启丢失；
2. `WebviewWindowBuilder` 创建 `main` 窗口，加载 `http://127.0.0.1:<port>/?token=<token>`；
3. 页面与浏览器版完全一致，仍走普通 HTTP/SSE，不使用 Tauri IPC；
4. 关闭窗口触发 `RunEvent::Exit`，中断仍在运行的 turn 后退出。
`app.windows` 为空，窗口由 `main.rs` 在服务就绪后创建；`capabilities/default.json` 只给
`main` 窗口 `core:default`，因为前端不调用 Tauri 命令。

## 页面加载顺序

`crates/harness/src/server.rs` 的 `load_index()`：

1. 优先读进程当前目录的 `web/index.html`（开发时改完刷新即生效）；
2. 再读当前目录的 `index.html`；
3. 再读编译机源码目录的 `web/index.html`；
4. 都不存在时使用 `include_str!("../../../web/index.html")` 编译期内置页面。

因此安装包只需分发 exe；不要求目标机器存在源码目录或 `web/` 文件夹。内置页面与打包时
的 `web/index.html` 同一份，重新改 HTML 后必须重新执行 Tauri 构建，安装包才会带上新页面。

## 默认工作区

首次启动且 `~/.parable/config.toml` 的 `workspace` 为空时，桌面壳使用
`~/.parable/workspace` 并自动创建。这样从开始菜单/快捷方式启动（工作目录通常是安装目录）
不会尝试往 `Program Files` 写文件；已有 `workspace` 配置保持不变。

## 构建

在仓库根目录执行：

```powershell
$env:PATH="$env:USERPROFILE\.cargo\bin;$env:PATH"
npx --yes @tauri-apps/cli@2.12.4 build --config crates/app/tauri.conf.json --bundles nsis --ci --no-sign
```

产物：

- `target/release/parable-app.exe`：桌面可执行文件；
- `target/release/bundle/nsis/Parable_0.1.0_x64-setup.exe`：NSIS 安装包。

`@tauri-apps/cli` 使用 2.12.4（npm 上不存在 2.12.5；`cargo tauri` 也未安装）。
`--no-sign` 仅用于本地测试包；正式分发需要代码签名证书。

## 安装器实测

静默安装到临时目录，启动安装后的 exe 检查窗口标题与进程存活，再运行卸载器清理。
安装器默认创建开始菜单快捷方式；Tauri NSIS 卸载器位于安装目录 `uninstall.exe`。
