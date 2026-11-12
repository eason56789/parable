# 架构说明

面向二次开发。UI 视觉层是既定资产（`web/index.html` 的 CSS 与 DOM 结构），不要重写；
以下是后端与接线的模块边界。

---

## 1. 事件契约（前后端唯一接口）

所有状态变化经 `EventHub`（`tokio::sync::broadcast`）fan-out 到每个 SSE 连接。
事件形如 `{type, sessionId, seq, ts, ...payload}`，`seq` 单调递增。

| type | payload | 前端动作 |
|---|---|---|
| `hello` | — | 握手，确认管道存活 |
| `session.start` | `title, workspace` | 建侧栏项，进入 chat 态 |
| `turn.start` | `turnId, model` | 建助手消息壳 |
| `reasoning.delta` | `text` | 追加进 `.th` 思考块（流式时保持展开） |
| `text.delta` | `text` | `swrite()` 增量写入 `.ab` |
| `tool.call` | `callId, tool, title, args` | 建工具卡片（`.ec`），置为运行中 |
| `tool.progress` | `callId, chunk` | shell 的流式输出 |
| `tool.result` | `callId, ok, title, display, output` | `fillToolCard()` 按 `display.kind` 渲染 |
| `permission.request` | `callId, tool, path, risk, diffPreview` | 在卡片上展开 `.pm` 三按钮面板 |
| `permission.resolved` | `callId, decision` | 收起面板 |
| `question` | `callId, question, options` | 展开提问输入框 |
| `status` | `turns, steps, tps, tokens{}, context{}` | 更新底部三栏 + 三个弹层 |
| `turn.end` | `ms, usage, stopReason` | `foldFlow()` 折叠成「已工作 Xs」 |
| `error` | `message, fatal` | 非致命进消息流，致命进 toast |

`display` 是工具卡片的结构化渲染载荷，`kind` 取 `read | diff | shell | ask | todo | plain`。
它随 `tool.result` 下发，也随消息持久化（`StoredMessage.displays`），这样重开会话能还原当时的卡片。

---

## 2. 模块边界

### `agent.rs` — 循环
```
loop {
  estimate_request() → 发 status
  provider.stream_chat(req) → 消费 StreamChunk
  累积 text/think/tool_calls → 推 history
  若有 tool_calls:
    只读工具 tokio::spawn 并行，写类严格串行
    每个工具：ToolCtx::authorize() → Registry::execute()
    全部结果合成「一条 user 消息」回填（Anthropic 要求）
  否则 break
}
```
- **取消**：`CancellationToken`，`tokio::select!` 穿插在流消费里
- **只读可并行**：`read_file / glob / grep / list_dir / web_fetch`
- **写类串行**：避免并发写冲突

### `provider/` — 协议适配
中立格式 `Msg::{User,Assistant}` + `AsstPart::{Text,Thinking,ToolUse}` + `UserPart::{Text,ToolResult}`。
每个 provider 负责转成自己的 wire format：

- `anthropic.rs`：`content_block_start/delta/stop`，`input_json_delta` 累积成完整 args，
  `signature_delta` 存 thinking 签名（回放 thinking 块必须带签名，否则丢弃该块）
- `openai.rs`：`chat/completions` 与 `responses` 两个变体共用 `OpenAiBase`，
  容忍 `reasoning_content` / 缺 `id` 等第三方差异

**流式生命周期**：`resp.bytes_stream()` → spawn 任务 → `mpsc` channel → `ReceiverStream`。
这样返回的 stream 是 `'static`，规避 reqwest stream 的生命周期推断问题。

### `tools/` — 执行与安全边界
所有路径经 `fs::resolve_within()`：`..` 归一化后必须落在工作区内，已存在路径再 `canonicalize()`
复查符号链接逃逸。工具返回 `ToolOutcome{ok, title, display, for_model}` ——
`title` 给卡片折叠行，`display` 给前端渲染，`for_model` 给模型（长输出截断但保留头尾并注明）。

### `permission.rs` — 网关
`auto_allow(mode, tool, path, workspace)` 先查规则表（`permissions.toml`），未命中再按 mode 判定。
需确认时 `oneshot` channel 挂起该 tool_call，HTTP `POST /api/permission` 唤醒。
`始终允许` 写规则表；5 分钟超时按拒绝；每次决策进 `logs/audit-<date>.jsonl`。

---

## 3. 鉴权

- 服务绑定 `127.0.0.1:0`（随机端口），每次启动生成 32 位 token
- `check_auth()` 在每个 handler 顶部执行，接受 `X-Parable-Token` 头或 `?token=` 查询（SSE 需要）
- `GET /` 注入 `window.__PARABLE_TOKEN`，前端所有 `fetch` 带头
- API Key 只在 `config.rs` 内流转，`get_config` / `get_providers` 输出前 `mask_key()`
- `merge_provider()` 收到含 `…`/`•` 的 key 会跳过写入，避免掩码回传覆盖真实密钥

---

## 4. 前端接线点

视觉层保留，JS 层改造处（对照 `web/index.html` 的 `<script>`）：

| 函数 | 改造 |
|---|---|
| `run()` | 删假编排，改 `POST /api/chat`，由 SSE 驱动 |
| `dispatch()` | 新增，按事件类型路由到渲染器 |
| `toolCard()/fillToolCard()` | 新增，通用工具卡片（替代原 `cards()` 的 read/edit 两种） |
| `showPermPanel()` | 权限三按钮 → `POST /api/permission` |
| `mkStream()/swrite()` | 新增，跨 chunk 保持 `**bold**`/`` `code` `` 解析状态 |
| `applyStatus()` | 状态栏由 `status` 事件驱动（原 `STATS` 是写死值） |
| `loadProviders()/mpDetail()` | 模型页接真实 CRUD + 测试连接 |
| `boot()` | 拉 config → apply → 连 SSE → 加载会话与模型 |

`window.__PARABLE_TOKEN` 或 `?token=` 都可作为凭据来源，便于浏览器调试。
