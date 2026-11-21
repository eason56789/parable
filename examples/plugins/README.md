# Parable 示例插件

让 Parable 的 Agent 直接操作本机 DCC 软件。每个插件是一个 manifest 包（`plugin.json` + 物化脚本），与 `~/.parable/plugins/` 的原生格式一致（见 `crates/harness/src/plugin.rs`）。

## 插件列表

| 目录 | 功能 | 通道 | 本机验证 |
|---|---|---|---|
| `blender/` | Agent 跑 bpy 脚本、建场景、导出、渲染帧 | 无头 `blender -b -P` | Blender 5.2.2 LTS（F 盘 Steam 库）✓ 端到端 ✓ |
| `aftereffects/` | Agent 执行 ExtendScript、查询安装、aerender 渲染 | **Startup 守护桥** + 收件箱轮询（AE 2025+ 的 `-r`/`-s` 命令行脚本通道已被 Adobe 废弃，实测脚本不执行；COM 也在 2024+ 移除） | AE 2026 ✓ 安装/渲染器探测 ✓；脚本执行需一次性开启写文件权限 |

### blender/ 工具

| 工具 | 作用 | 上限 |
|---|---|---|
| `blender_info` | 探测 blender.exe（PATH → Program Files → 各盘 Steam 库）与版本 | 60s |
| `blender_run` | 无头执行 bpy Python（可先打开 .blend），print 什么 Agent 就看到什么 | ~105s |
| `blender_render` | 渲染帧区间（`-s/-e/-o/-a`），引擎与分辨率取工程设置 | ~105s |

### aftereffects/ 工具

| 工具 | 作用 | 上限 |
|---|---|---|
| `ae_info` | 探测 AfterFX.exe / aerender.exe 版本 + 桥接/权限自检（不启动 AE） | — |
| `ae_script` | 执行 ExtendScript（ES3！）：自装 Startup 守护桥 → 收件箱投递载荷 → 结果文件取回；AE 未运行时自动启动 | 100s 可调至 110 |
| `ae_render` | aerender 渲染工程，支持 comp/帧区间/RS·OM 模板 | ~110s |

**AE 一次性设置**（`ae_script` 依赖，超时诊断会逐条提示）：

1. AE 「设置 > 脚本和表达式 > **允许脚本写入文件和访问网络**」开启——守护桥读收件箱、载荷写结果都靠它。
2. AE 启动时若弹模态警告框，点掉（内容可转述给 Agent）。

两个插件都有 `/blender`、`/ae` 斜杠命令与 system prompt 注入（告诉 Agent API 习惯与超时策略）。

## 安装

```bash
cd examples/plugins
node install.mjs            # 装全部到 ~/.parable/plugins
node install.mjs blender    # 只装一个
node install.mjs --home D:\my-parable-home
```

安装器做的事：拷贝 manifest → 把 `code` 物化成 `scripts/<tool>.<ext>` 并回填 `script` 绝对路径与 `command`（与 `plugin.rs::write_script` 完全同格式）→ 在 `enabled.json` 登记启用。

然后**重启 Parable**（或在「设置 → 插件」里切换一次开关）即可。工具会出现在 Agent 的工具列表，首次调用走 Shell 权限确认。

## 为什么要物化脚本

Parable 的 `plugin_create`（Agent 自己写插件）会在创建时把内联代码落盘；但手写 manifest 不会。`install.mjs` 就是手写包的落盘器。若不想跑安装器，也可以在 Parable 对话里直接说「帮我创建一个 blender 插件」，用 `plugin_create` 让 Agent 自己装。

## 安全说明

- 工具执行走 Parable 权限网关（Shell 风险级），首次调用需确认；「始终允许」按渲染后的命令精确匹配。
- `blender_run` / `ae_script` 的 code 参数由模型生成、在宿主软件里执行——效果等同本机跑任意 Python/ExtendScript，请只在你自己的机器上使用。
- 内联脚本参数经 `PARABLE_TOOL_ARGS` 环境变量传递（JSON），不经命令行引号转义；环境变量按 `CHILD_ENV_WHITELIST` 白名单下发。

## 已知限制

- 插件工具层 120 秒硬超时（`plugin.rs` 里的 `tokio::time::timeout`）。长渲染请分帧区间调用，或让 Agent 用 bash 工具把同一命令挂到后台终端（`/api/term/start`，不受此限制）。
- `ae_script` 超时后 AE 可能仍在冷启动/执行，重试通常秒回；AE 弹模态对话框会卡住脚本，需要人工点掉。
- Blender 探测只扫 `C:`–`H:` 盘的 Steam 常见布局；更奇怪的位置用 `blender_path` 参数手动指定。
