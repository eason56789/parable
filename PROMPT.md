# 提示词：把 Parable 单文件 HTML 复刻改造成真实可用的 AI Harness（对标 Claude Code / Codex / ZCode）

---

## 0. 角色

你是一名资深全栈/系统工程师，熟悉 Rust 异步运行时、LLM 流式协议、Agent 工具循环、权限模型与桌面应用架构。你的任务不是"做一个聊天界面"，而是**交付一套真实能干活、可执行代码、可操控文件、可自我校验的 Agent Harness**，用户界面只是它的显示器。

你必须一次交付可编译、可运行、可验证的完整工程。**禁止留下任何"模拟/占位/假数据/TODO"路径。**

---

## 1. 目标与验收定义

最终产物是一个桌面端应用（Rust 核心 + webview 承载现有 HTML UI），具备：

- 真实调用 LLM（Anthropic Messages / OpenAI Responses / OpenAI Chat Completions 三种协议，Base URL 可配），**流式**输出文本、思考（reasoning/thinking）片段与工具调用。
- 真实的 Agent 循环：模型 → 工具调用 → 结果回填 → 继续，直到模型给出最终答复。
- 真实工具集：读写文件、编辑文件（带 diff）、列目录、glob、grep、shell、网页抓取、待办计划、询问用户。
- 真实权限网关：三种权限模式（每次询问 / 工作区免询问 / 完全放开），带"允许一次 / 始终允许 / 拒绝"与可持久化的授权规则。
- 真实上下文管理：token 计量、上下文占用可视化、溢出时自动压缩（compaction）。
- 真实会话持久化：多会话、置顶/重命名/归档/删除/搜索、Markdown 导出、重启后可恢复。
- 真实设置与模型供应商配置：真实写入 API Key、Base URL、模型列表，真实测试连通性。
- 真实状态栏数据：轮次、步数、tok/s、缓存命中率、上下文分项占用。
- 进阶能力（可分阶段，但最终必须全部可用）：MCP 客户端、Skills、Hooks、子代理（并行）、计划模式。

**一句话验收**：用户在界面里说"读一下 Cargo.toml，把依赖升级到最新并跑测试"，Agent 会真实读取文件、生成 diff、在权限面板请求批准、写回文件、执行 `cargo test` 并把输出讲清楚。**全过程无任何伪造输出。**

---

## 2. 输入物料与现状勘察结论（重要，请先读代码再动手）

**唯一 UI 素材**：`D:\下载\deepseek_html_20261112_45322b (1).html`（1355 行，单文件，内联 CSS + 内联 JS，无构建、无外部依赖除 Google Fonts）。
把它**原样保留**为前端，只改造 JS 层接入真实后端。**禁止重写视觉层**。

### 2.1 必须原样保留（不要动）

| 位置 | 内容 |
|---|---|
| L.482–516 | `IC` 图标表、`ic()`、`spark()` |
| L.9–384 | 全部 CSS（`--bg/--side/--fg/--line/--accent` 变量体系、`.ec/.df/.dc` diff 视图、`.pm` 权限面板、`.wf/.wfb` 折叠流程、`.sbtn/.spanel` 状态栏弹层） |
| L.577–617 | `inline()` / `paint()` / `blocks()` 流式渲染器（**这是核心资产**，只需改造其数据来源） |
| L.636–666 | `cards()` 的**视觉结构**（read 卡 / edit 卡 / diff 高亮 / 权限三按钮 `.pm`） |
| L.680–701 | `uMsg()` / `aMsg()` 消息与助手气泡 DOM |
| L.702–708 | `think()` 思考块结构（`.th/.thb/.shim`） |
| L.714–729 | `foldFlow()`「已工作 {t}」折叠流程 |
| L.482–528 | `L.zh` / `L.en` 双语词表结构（保留 key，替换 `r.*` 与 `dc` 的假数据） |
| L.1045–1060 | `exportChat()` Markdown 导出 |

### 2.2 必须删除 / 替换（演示层）

| 位置 | 现状 | 处理 |
|---|---|---|
| L.622 `pick(q)` | 关键词匹配挑 canned key | **删除**，改为服务端事件驱动 |
| L.623–632 `ED` | 三段假文件内容与假 diff | **删除**，改为真实 read/edit 事件 |
| L.674–678 `edit()` | 假的权限等待 | 重写为真实 `permission.request` → 用户决策 |
| L.730–747 `run()` | 假的一轮对话编排 | 重写为 SSE 事件驱动的真实 turn |
| L.517/525 `L.*.r` | 三段假回复 | **删除**（改为模型真实输出） |
| L.518/525 `L.*.dc` | 4 条种子会话 | **删除**，改为从数据库读取（可留空列表） |
| L.985–1036 `STATS` | 写死的统计数字 | 改为服务端 `status` 事件推送 |
| L.1267 `MPROV` | 假供应商 + 假 API Key | 改为真实配置存储 |
| L.534 `PK` / L.536 `MN` | 权限 key / 模型名 | 保留 key，值改为动态（来自后端） |
| L.529 `PG` | 4 个占位页面 | 至少让 Projects/Artifacts 页读取真实数据；Chats 页指向侧栏 |
| L.531–532 `localStorage 'cd-s'` | 设置存 localStorage | **迁移到后端配置文件**，localStorage 仅作缓存镜像 |
| L.1106–1118 `cmdActions()` | 静态命令 | 扩充真实命令（见 §13） |
| L.1064–1078 `slashList()` | 9 条静态斜杠命令 | 扩充真实斜杠命令（见 §13） |

### 2.3 需要扩展的既有 UI

- `.ec` 工具卡片目前只有 read/edit 两种 → 需支持 **bash / grep / glob / web / mcp / subagent / todo** 等类型的卡片渲染（沿用同一套 `.ec > .eh > .dc` 结构与折叠交互，新增类型图标与摘要行）。
- `.pm` 权限面板目前固定文案 → 需动态渲染工具名、目标路径、diff 预览、风险说明。
- `#sbar` 三栏 → 全部接真实数据；上下文分项条（系统提示词 / 工具定义 / 对话消息）需真实占比。

---

## 3. 技术栈与硬约束

**后端**
- Rust 2021 edition，稳定版工具链。
- 异步运行时 `tokio`（full features）。
- HTTP 层 `axum`（v0.7+）。SSE 手写 `axum::response::sse` 或 `tokio-stream`，**不要引入重量级 SSE 框架**。
- HTTP 客户端 `reqwest`（`rustls-tls` 或 `native-tls` 均可；`stream` feature 必开）。
- 序列化 `serde` / `serde_json`；错误 `thiserror` + `anyhow`。
- 日志 `tracing` + `tracing-subscriber`（滚动文件 + stderr）。
- 持久化 `rusqlite`（bundled feature，免系统依赖）存会话、消息、工具调用、token 统计；配置文件与凭据用 `dirs` 定位 `~/.parable/`。
- 搜索 `ignore` + `grep` crate（实现 ripgrep 级 `grep`），glob 用 `globset`，遍历用 `walkdir` 或 `ignore::Walk`。
- diff 生成用 `similar` crate 生成 unified diff（用于前端 diff 卡片与 patch 应用校验）。
- token 计量用 `tiktoken-rs`（若某模型无对应编码，用启发式：CJK 按 ~1.6 token/字，英文 ~4 字符/token，并允许服务端 usage 覆盖）。
- 桌面壳：**Tauri v2**（`tauri` crate + `tauri-build`）。Tauri v2 若在目标环境不可用，**允许降级为 `wry` + 自建窗口**，但必须保留"Rust 侧起本地 HTTP 服务、webview 加载 `http://127.0.0.1:<port>`"这一架构。

**前端**
- 保持单文件 HTML，仅改 JS 层。**不引入打包器、不引入 npm 依赖、不改 CSS**。
- 保留现有 `apply()` / 设置体系，但设置项的读写改为调用后端。
- 与后端通信用 `fetch` + `EventSource`（SSE）。

**硬约束**
- 后端只监听 `127.0.0.1`，端口随机；每次启动生成随机会话 token，**所有 `/api/*` 请求必须校验 token**（Header `X-Parable-Token` 或查询参数），并做 **Origin 白名单校验**（只允许 `tauri://localhost`、`http://tauri.localhost`、`http://127.0.0.1:<port>`）。
- API Key 只落 `~/.parable/credentials.json`（文件权限 `0600`），**永不下发给前端**，前端只显示掩码 `sk-…abcd`。
- 前端 `EventSource` 无法自定义 Header → token 用查询参数传递，并在服务端做一次性校验 + 标记已用会话。

---

## 4. 进程与架构总览

```
┌─ Tauri 桌面进程 (main.rs)
│   ├─ 启动 axum: 127.0.0.1:<random>  ← 事件中心 / REST
│   ├─ 加载 web/index.html（= 改造后的原始 HTML）到 webview
│   └─ 生命周期：关闭窗口时中止所有运行中的 Agent，flush 日志
│
├─ Harness Core (src/harness/)
│   ├─ agent_loop.rs   : 单会话 agent 循环、取消、并发控制
│   ├─ prompt.rs       : 系统提示词装配、上下文预算、compaction
│   ├─ providers/      : anthropic.rs / openai_responses.rs / openai_chat.rs
│   ├─ tools/          : fs.rs / shell.rs / search.rs / web.rs / plan.rs / agent.rs
│   ├─ permission.rs   : 权限规则引擎 + 授权记忆
│   ├─ session.rs      : 会话/消息/工具调用持久化
│   ├─ mcp.rs          : MCP 客户端（stdio + http/sse）
│   ├─ skills.rs       : Skill 发现与加载
│   ├─ hooks.rs        : 生命周期钩子
│   └─ subagent.rs     : 子代理（隔离上下文、并行）
│
└─ Webview (web/index.html)
    EventSource(/api/events) ← 所有状态变化
    fetch(/api/*)           ← 所有用户动作
```

**事件中心（EventHub）**：一个 `tokio::sync::broadcast` 通道，服务端所有状态变化 publish，前端每会话一条 SSE 连接。事件必须带 `sessionId` 与单调递增 `seq`，前端据此丢弃乱序事件。

---

## 5. 前后端契约（必须严格实现，前端提示词也要按此写）

### 5.1 SSE 事件：`GET /api/events?token=...&sessionId=...`

服务端 → 前端，单条 JSON：

```jsonc
{ "type": "...", "sessionId": "s_xxx", "seq": 12, "ts": 1731400000000, ...payload }
```

| type | payload | 说明 |
|---|---|---|
| `session.start` | `{title, workspace}` | 会话就绪 |
| `turn.start` | `{turnId, model}` | 一轮开始 |
| `reasoning.delta` | `{turnId, text}` | 思考增量（喂给 `.shim` → 思考块） |
| `reasoning.done` | `{turnId, text, ms}` | 思考结束，落盘 |
| `text.delta` | `{turnId, text}` | 正文增量（喂给 `paint()`） |
| `text.done` | `{turnId, text}` | 正文结束 |
| `tool.call` | `{turnId, callId, tool, title, args}` | 工具即将执行，`title` 是卡片摘要行 |
| `tool.progress` | `{turnId, callId, chunk}` | 长工具的增量输出（shell 流） |
| `tool.result` | `{turnId, callId, ok, output, durationMs, error}` | 工具结束 |
| `file.read` | `{callId, path, lines, content}` | 渲染 read 卡片（带行号） |
| `file.diff` | `{callId, path, hunks:[{oldStart,oldLines,newStart,newLines}], added, removed}` | 渲染 edit 卡片 diff |
| `permission.request` | `{callId, tool, path, risk, reason, diffPreview}` | **弹权限面板，等待前端回复** |
| `permission.resolved` | `{callId, decision}` | 决策回执（前端也可自回执） |
| `status` | `{turns, steps, tps, tokens:{total,cacheHit,uncached,cacheRead,output}, context:{used,limit,system,tools,messages}}` | 驱动底部状态栏三栏 |
| `turn.end` | `{turnId, ms, usage, stopReason}` | 一轮结束，更新"已工作 Xs" |
| `subagent.start` / `subagent.end` | `{agentId, name, status}` | 子代理 |
| `plan.updated` | `{todos:[{id,content,status}]}` | 计划面板 |
| `error` | `{message, fatal}` | 非致命错误进消息流，致命错误进 toast |

### 5.2 REST：`POST /api/*`（Header 必须带 `X-Parable-Token`）

| 方法 | 路径 | body / 说明 |
|---|---|---|
| POST | `/api/chat` | `{text, attachments:[{path,name,size}], model, permissionMode, thinkingLevel}` → 触发一轮，返回 `{turnId}` |
| POST | `/api/interrupt` | 中止当前轮（前端 stop 按钮） |
| POST | `/api/permission` | `{callId, decision:"once"\|"always"\|"deny"}` |
| POST | `/api/session/new` | 新建会话 `{workspace}` |
| GET | `/api/sessions` | 会话列表（含 pin/archive/时间） |
| PATCH | `/api/sessions/:id` | `{title?, pinned?, archived?, group?}` |
| DELETE | `/api/sessions/:id` | 删除 |
| GET | `/api/sessions/:id/messages` | 历史消息（含工具调用卡） |
| GET | `/api/config` / PATCH | 全局设置（主题/字体/动效可留在前端，但后端也要一份） |
| GET | `/api/providers` / POST / PATCH / DELETE | 供应商 CRUD（**只回掩码 key**） |
| POST | `/api/providers/:id/test` | 真实发一次最小请求，返回延迟/错误 |
| GET | `/api/models` | 可用模型列表（由供应商汇总，供模型选择器使用） |
| GET | `/api/workspace` | 当前工作区信息 + 文件树根 |
| GET | `/api/attachments/preview?path=` | 附件预览（文本类按行取，限制大小） |
| GET | `/api/tools` | 工具清单 + JSON Schema |
| GET | `/api/skills` / `/api/mcp/servers` | 能力清单 |
| POST | `/api/agent/interrupt` | 中止子代理 |

---

## 6. Agent 循环规格

```
loop {
  1. 装配请求：system prompt + 工具 schema + 历史消息 + 当前轮用户消息
  2. 依据上下文预算决定是否 compaction（§9）
  3. 流式调用 provider
  4. 文本增量 → emit text.delta ；思考增量 → emit reasoning.delta
  5. 若模型返回 tool_calls：
       a. 逐个（可并行，见下）生成 permission.request（若命中免询问规则则跳过）
       b. 拿到决策后执行工具 → emit tool.call / tool.progress / tool.result / file.read / file.diff
       c. 把 tool_result 以 provider 要求的格式回填到消息历史（Anthropic: user+tool_result；OpenAI: tool message）
       d. 继续 loop
  6. 若模型给出最终文本 → emit text.done，结束本轮
  7. 达到最大步数（如 100）或被中断 → 结束并告知用户
}
```

- **取消**：`tokio::select!` + `CancellationToken`（`tokio_util::sync::CancellationToken`）。中断时立刻停止流、终止子进程、把已产生的部分文本落盘并回显给用户。
- **只读工具可并行**：`read_file` / `glob` / `grep` / `list_dir` 可并发；写类工具**必须串行**（避免并发写冲突），且串行队列在单会话内。
- **每个 tool_call 独立计步**，`steps` 统计来自这里。

---

## 7. 工具集规格（必须真实实现）

**文件系统**
| 工具 | 参数 | 行为 |
|---|---|---|
| `read_file` | `path, offset?, limit?` | 读文件，返回带行号内容；二进制/超大要明确报错；默认单次上限 2000 行 / 256KB |
| `write_file` | `path, content` | 整文件写入（覆盖），写前若有旧内容 → 生成 diff 事件 |
| `edit_file` | `path, old_string, new_string, replace_all?` | 精确字符串替换；`old_string` 不唯一或不存在 → 返回明确错误让模型自纠 |
| `apply_patch` | `path, hunks[]` | 应用 unified diff（可选实现，若做则需严格校验） |
| `list_dir` | `path?` | 列目录（含大小、类型、忽略隐藏项） |
| `glob` | `pattern, path?` | 文件匹配 |
| `grep` | `pattern, path?, glob?, ignore_case?, max_results?` | 内容搜索（ripgrep 语义），输出 `file:line:content` |
| `create_dir` / `move_file` / `delete_file` | `path` | `delete_file` 走破坏性风险判定，必须走权限确认 |

**执行**
| 工具 | 参数 | 行为 |
|---|---|---|
| `bash` | `command, timeout_ms?, cwd?` | `sh -c` 执行；**流式输出**（`tool.progress`）；超时杀进程组；返回 exit code + stdout + stderr（截断上限，保留尾部） |

**网络**
| 工具 | 参数 | 行为 |
|---|---|---|
| `web_fetch` | `url, max_bytes?` | 抓取并转成 Markdown（去脚本/样式/导航），返回正文 |
| `web_search` | `query, limit?` | 走可配置的搜索 API（Tavily/Brave/SerpAPI 或自定义 endpoint），无配置时返回"未配置搜索"错误而非伪造 |

**规划与交互**
| 工具 | 参数 | 行为 |
|---|---|---|
| `todo_write` | `todos[]` | 更新计划（`plan.updated` 事件 + 前端计划面板） |
| `ask_user` | `question, options[]?` | 向用户提问并等待（复用 permission.request 的等待通道，或独立 `question` 事件） |
| `spawn_agent` | `name, task, model?` | 派生子代理，独立上下文 |
| `skill` | `name, args?` | 加载一个 Skill 的完整指令进上下文 |

**MCP 工具**：以 `mcp__<server>__<tool>` 形式动态注册，schema 转成同一套 JSON Schema。

**所有工具的共同要求**：
- 严格 JSON Schema 校验入参，错误信息要能指导模型自我修正（"expected string, got number" 这类）。
- 路径解析：相对工作区 → 绝对路径；**必须做越界检查**（符号链接、`..`），越界时按权限模式处理（ask → 请求授权；ws/full → 允许或拒绝并说明）。
- 工具输出要**为模型优化**：长输出截断但保留头尾并注明"已截断，共 N 行"；错误要包含可操作信息。

---

## 8. 权限模型规格

三种模式沿用现有 key：`ask` / `ws` / `full`（`L.zh` 里 `p_ask`/`p_ws`/`p_full` 与 `d_ask`/`d_ws`/`d_full` 文案保留）。

| 模式 | 行为 |
|---|---|
| `ask`（每次编辑前先征求同意） | 所有**写类 / 破坏性 / 网络 / 工作区外**工具都请求确认；只读工具免确认 |
| `ws`（工作区内直接读写，区外询问） | 工作区内读写免确认；工作区外写入、删除、shell 提权、网络请求仍确认 |
| `full`（不再询问，请谨慎使用） | 全部自动放行，但仍记录完整审计日志，并在 UI 上以醒目色提示当前是高危模式 |

**决策回路**：
1. 权限引擎按 `(tool, 规范化路径, 动作类型)` 查规则表。
2. 命中 `allow` 规则 → 直接执行；命中 `deny` 规则 → 直接返回拒绝并告知模型。
3. 未命中 → `emit permission.request` → **挂起该 tool_call**（`tokio::sync::oneshot` 等待）→ 前端 POST 决策 → 唤醒继续。
4. 决策 `always` → 写入规则表持久化到 `~/.parable/permissions.toml`（形如 `[[rule]] tool="edit_file" path="src/**" action="allow"`），并 toast 提示已记住。
5. 前端若在 5 分钟内未响应 → 超时按 `deny` 处理，并在消息流中说明"权限请求超时已跳过"。

**风险分级**：卡片上用不同色标提示（低危/中危/高危，高危如 `rm -rf`、`delete_file`、工作区外写入、`curl | sh`）。

**审计日志**：`~/.parable/logs/audit-<date>.jsonl`，记录每次工具调用的决策与结果。

---

## 9. 上下文与令牌管理

- **计量**：分别统计 `system`（系统提示词）、`tools`（工具 schema）、`messages`（历史消息与工具结果）三部分 token 数，以及总占用与模型上下文上限（`ctxLimit`，可按模型配置，默认 128K/1M）。
- **缓存命中**：provider 返回 `cache_read_input_tokens` / `cache_creation_input_tokens` 时统计真实命中率；未返回时用"未缓存输入 vs 缓存读取"的估算比。
- **TPS**：按文本增量的字符/token 数 ÷ 产出耗时实时算，移动平均。
- **压缩（compaction）**：当 `used / limit > 0.85` 触发——
  1. 保留 system + 最近 N 轮原文；
  2. 把更早的历史交给模型做一次**摘要**（专用 summarizer prompt，产出结构化摘要：已完成的事、已改动的文件、关键决策、待办）；
  3. 用摘要替换被压缩区间，并写入一条 `compaction` 系统消息（前端要能显示"已压缩 N 轮上下文"）。
- **单条工具结果上限**：超过阈值先截断；超大文件不整份塞进上下文，只塞片段 + 行号范围。

---

## 10. 会话与数据持久化

**存储位置**：`~/.parable/`
```
config.toml            # 全局设置
credentials.json       # API Key（0600）
permissions.toml       # 授权规则
sessions.db            # SQLite：sessions / messages / tool_calls / usage
logs/                  # tracing + audit jsonl
skills/                # 用户自定义 skill（也可从工作区 .parable/skills 发现）
```

**Schema（要点）**：`sessions(id, title, workspace, created_at, updated_at, pinned, archived, group, model, permission_mode, thinking_level)`；`messages(id, session_id, role, content, reasoning, created_at, seq)`；`tool_calls(id, message_id, session_id, tool, args, result, status, started_at, ended_at)`；`usage(session_id, turn, tokens_*, cache_*, model)`。

**行为要求**：
- 新建会话 = 写入 sessions 行 + 前端立刻加入侧栏「最近」列表（`addRc` 复用现有 DOM）。
- 侧栏增删改查全部走 REST，前端不再持有权威数据。
- **重启后恢复**：打开应用加载最近会话列表；点开会话时通过 `/api/sessions/:id/messages` 重建完整消息流（含工具卡片与折叠流程）——即 `open(c)` 现有函数从"读内存"改为"拉服务端 + 渲染"，渲染逻辑复用现有 `uMsg/aMsg/cards/paint/foldFlow`。
- 搜索：`GET /api/sessions?q=`（SQLite LIKE + FTS5 若可用），结果走现有命令面板。
- 导出：现有 `exportChat()` 改为从服务端拉完整消息（含工具调用）再拼 Markdown。

---

## 11. Provider 与模型配置页（让 §2.2 里那个假页面变成真的）

现有 `MPAPI` 三种协议保持不变，直接变成真实实现目标：

| API 格式 | 端点 | 要实现的细节 |
|---|---|---|
| `anthropic` | `{base}/v1/messages` | `x-api-key` + `anthropic-version` 头；`stream:true`；解析 `content_block_delta`（text / thinking / input_json_delta）；工具用 `tool_use` + `tool_result`；支持 `cache_control` 缓存标记以命中 prompt cache |
| `responses` | `{base}/v1/responses` | OpenAI Responses 协议；`function_call` / `function_call_output`；reasoning summary |
| `openai` | `{base}/v1/chat/completions` | `tools` + `tool_calls` + `tool` role 回填；`stream_options.include_usage` |

**供应商配置项**：名称、Base URL、API 格式、API Key（掩码显示）、模型列表（每项含名称、上下文上限、是否支持视觉/工具、启用开关）、启用开关、颜色标识。
**必须真实可用的操作**：
- 「获取 API Key」→ 打开对应厂商控制台/文档页（新窗口），不弹假 toast。
- 「刷新」→ 真实调用厂商的模型列表端点（若厂商支持），失败则如实报错。
- 「测试连接」→ 真实发一个最小请求（如 1 token 的 `ping`），返回延迟、是否成功、错误原文。
- 新增/删除供应商、模型增删改、启用开关 → 全部持久化。
- 顶部模型选择器（`#mm` 弹层）改为从 `/api/models` 动态渲染，而非写死的 `MN`。

**兼容性要求**：Base URL + Anthropic 协议必须能直接对接 DeepSeek / Kimi / GLM 等国内厂商的 Anthropic 兼容端点与国内中转站。

---

## 12. 前端接线表（逐处改造）

| 现有函数 | 改造要求 |
|---|---|
| `run(q)` | 删掉全部假逻辑：改为 `POST /api/chat` 后由 SSE 事件驱动渲染；保留 `stick/follow` 滚动跟随与 `busy/sig` 停止语义 |
| `edit(ab,k,sg)` | 删掉；权限等待改由 `permission.request` 事件 → 前端渲染 `.pm` 面板 → `POST /api/permission` → `permission.resolved` 事件确认 |
| `cards(ab,k,st)` | 保留 DOM 模板，改成**通用工具卡片渲染器** `renderToolCard(tool, title, meta)`：按 `tool` 选择图标与摘要行；read 类渲染 `file.read` 的行号内容；edit 类渲染 `file.diff` 的 hunks（`+`/`-` 高亮沿用 `.df .ad/.rm`）；bash 类渲染 stdout/stderr；其余用通用输出区 |
| `think(a,sg)` | 改为由 `reasoning.delta` 增量写入 `.thb`，`reasoning.done` 时写入"思考了 {n} 秒" |
| `foldFlow(a,ms)` | 保留，时长取 `turn.end.ms` |
| `paint()` / `inline()` | 保留，改为由 `text.delta` 增量喂入；`text.done` 时完成收尾（代码块编号、复制按钮） |
| `addRc(c)` / `sortRcs()` / `refreshTimes()` | 保留，列表数据源改为 `/api/sessions` |
| `open(c)` | 改为拉服务端消息重建；重放工具卡片与思考块 |
| `openChatMenu()` / `CM` 事件 | 各菜单项接真实 API：置顶/未读/重命名/归档/删除/复制路径/复制会话 ID；"打开配置"跳到设置页；"查看调用轨迹"打开该会话的 tool_calls 详情 |
| `exportChat()` | 改为从服务端拉全量消息再拼 Markdown |
| `STATS` / `updateStatus()` | 删掉写死值，改为监听 `status` 事件更新三栏 + 三个弹层 |
| `MPROV` / `mpList()` / `mpDetail()` | 改为 CRUD 真实供应商；`mp_getkey` 打开真实链接；`mprefresh` 真实拉模型列表；新增"测试连接"按钮 |
| `MN` / `renderMM()` | 模型列表改为 `/api/models` |
| `slashList()` | 扩充：`/clear` `/export` `/model` `/perm` `/think` `/compact` `/init` `/plan` `/agents` `/skills` `/mcp` `/status` `/cost` `/help` |
| `cmdActions()` | 扩充同上加"新建/切换工作区/查看日志/打开设置/查看任务目录"等 |
| `apply()` | 保留全部视觉逻辑（主题/字体/动效），但持久化目标改为 `PATCH /api/config`（localStorage 作为乐观缓存） |
| `#pg` 页面 | Projects 页列真实工作区；Artifacts 页列会话产出的文件/代码块产物（可先实现"本会话产出的代码块与文件"） |
| `PG` 文案 | 占位说明文案改为真实功能描述 |

**新增前端能力（沿用现有视觉语言）**：
- 顶部工具栏显示**当前模型 + 权限模式 + 思考强度**，与后端状态一致（`#tb2`、`#ml`、`#pml` 已有位置，改为动态值）。
- 工作区指示（当前 workspace 路径）。
- 计划模式（Plan Mode）开关：开启后模型只允许只读工具与 `todo_write`，直到用户切回执行模式。
- 中断提示与错误提示接入 `#toast`。

---

## 13. 进阶能力（最终必须可用）

**MCP 客户端**：配置 `~/.parable/mcp.json` 或工作区 `.parable/mcp.json`，支持 `stdio` 与 `http/sse` 两种传输；列出 server 状态（连接中/已连接/失败 + 错误原文）；把工具以 `mcp__server__tool` 注册进工具集；调用走同样的权限网关。

**Skills**：支持 `~/.parable/skills/<name>/SKILL.md` 与工作区 `.parable/skills/`，YAML frontmatter（`name`/`description`/`allowed-tools`）。`description` 注入系统提示词供模型按需调用；调用时把 SKILL.md 正文注入上下文并记录来源。UI 要能列出已加载 skill 及其描述。

**Hooks**：配置 `config.toml` 中 `[[hook]] event="pre_tool_use"|"post_tool_use"|"pre_prompt"|"session_end"` 与 `command`；执行后其 stdout 作为附加上下文或阻断（pre_tool_use 非零退出 → 阻断并把 stderr 给模型）。失败要如实上报。

**子代理**：`spawn_agent` 创建隔离子代理（独立 system prompt + 独立上下文窗口 + 独立权限模式），并行执行多个时用 `tokio::join!`，结果以摘要回填父代理；UI 用 `.ec` 卡片展示子代理状态与最终产出。

**成本**：若 provider 返回 cost 字段则累计；否则按本地价格表（可配置）估算，在 `/cost` 命令与状态面板展示。

**CLI 模式（可选加分）**：同一个 Rust 二进制同时提供 `parable -p "任务" --workspace <dir>` 无头模式，复用完全相同的 harness 核心，便于脚本化与 CI 使用。

---

## 14. 质量要求

- `cargo build --release` 必须零 warning 通过（`#![deny(warnings)]` 视情况，但不允许残留 `unused`）。
- `cargo test` 全绿，至少覆盖：
  - 路径越界与符号链接逃逸的拦截；
  - 权限规则匹配（含 `always` 规则持久化与命中）；
  - Anthropic / OpenAI 两种 SSE 流的解析（含分片 JSON 拼接、工具参数增量）；
  - `edit_file` 的唯一性校验与 diff 生成；
  - compaction 触发阈值与摘要替换；
  - 会话 CRUD 与消息重建。
- 集成测试用本地 mock LLM 服务（`wiremock` 或手写 axum mock），返回固定 tool_call 序列，验证「工具 → 结果回填 → 第二轮」链路。
- 错误处理：**禁止 panic 逃逸到 HTTP 层**；所有错误转成结构化响应；模型看到的是"可自我纠正的错误描述"而不是堆栈。
- 日志：`tracing` 分级（`RUST_LOG` 控制），默认 info；工具调用记录 `call_id / tool / 耗时 / 结果大小`。
- 安全：token 校验、Origin 校验、命令执行禁用交互式 shell（`sh -c` 非 login）、超时与进程组回收、输出长度上限、API Key 不回传前端、不写日志明文。

---

## 15. 非目标（明确不要做）

- 不做多用户账号体系、团队协作、云同步、插件市场。
- 不做移动端原生 App（现有 CSS 的移动断点保留即可）。
- 不重写视觉设计，不引入 UI 框架、状态管理库、打包器。
- 不为了"看起来完成"而保留任何假数据或 mock 响应（**唯一允许的 mock 只出现在测试代码里**）。

---

## 16. 交付顺序（按阶段推进，每阶段可独立验证）

1. **P0 骨架**：Cargo 工程、Tauri/ wry 外壳、axum 服务 + token 校验、静态服务改造后的 HTML、EventHub 与 SSE 通道、前端 `EventSource` 接入并能收到 `session.start` / `status`。
2. **P1 打通一条真实链路**：provider（先只做 Anthropic 协议）→ 文本流 → `text.delta` → 前端逐字渲染；模型列表与 API Key 配置真实化。
3. **P2 工具与权限**：全部工具实现 + 权限网关 + 前端权限面板 + diff 卡片真实化；跑通"读文件 → 改文件 → 跑 shell"。
4. **P3 会话与上下文**：SQLite 持久化、侧栏 CRUD、消息重建、token 计量、状态栏真实数据、compaction。
5. **P4 进阶**：MCP、Skills、Hooks、子代理、计划模式、CLI 模式（可选）。
6. **P5 加固**：测试、安全审查、错误边界、README（架构图 + 使用说明 + 二次开发指引）。

每一阶段结束时，你必须输出：**改动了哪些文件、怎么验证、验证命令与实际输出**。不允许只贴代码不贴验证结果。

---

## 17. 最终验收清单（逐条打勾，不许含糊）

- [ ] 断网状态下打开应用，UI 完整可用，无 JS 报错
- [ ] 配置真实 DeepSeek/Anthropic Key 后，能流式收到真实回复
- [ ] 让 Agent 读取工作区文件，前端 read 卡片显示真实内容与行号
- [ ] 让 Agent 修改文件，前端弹出权限面板（允许一次/始终允许/拒绝三按钮），diff 高亮与真实改动一致
- [ ] "始终允许"后重复同类操作不再弹窗，且规则重启后仍在
- [ ] 让 Agent 执行 `ls` / `grep` / `cargo test`，输出为真实命令结果
- [ ] 中断生成后模型立刻停下，已生成内容保留且不残留孤儿进程
- [ ] 关闭并重开应用，侧栏会话仍在，点开能完整重建（含工具卡片与思考块）
- [ ] 状态栏三栏数字与真实用量吻合；上下文条随对话推进而增长
- [ ] 长对话触发压缩，前端显示压缩提示，模型仍记得早期关键事实
- [ ] 模型配置页新增供应商、填 Key、测试连接成功，模型选择器随之变化
- [ ] `/compact`、`/plan`、`/export` 等斜杠命令行为正确
- [ ] `cargo build --release` 与 `cargo test` 全部通过
- [ ] 源码中搜索不到任何遗留的假数据分支（`pick(`、`ED`、`STATS` 写死值、`L.*.r`）——请在交付说明里附上这次搜索的结果

---

## 18. 工作方式要求

1. **先读代码再动手**：第一件事是把 1355 行的 HTML 通读并画出模块清单，确认 §2 的替换点与你的理解一致。
2. **先给架构再写码**：先产出模块划分、事件契约、数据流图（可写在 `docs/ARCHITECTURE.md`），再开始实现。
3. **小步提交**：按 §16 的阶段推进，每阶段结束时给出可运行的验证方式。
4. **如实汇报**：测试失败就贴失败输出；某功能未完成就明确写"未完成"及原因，**禁止用"已实现（简化版）"这类含糊表述蒙混**。
5. **遇到需要你决策的分歧**（如 provider 协议细节、目录结构偏好），列出选项与推荐项，让用户拍板，不要自行拍板后不说明。
