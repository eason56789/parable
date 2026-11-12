# Parable — 本地 AI Harness

一个对标 Claude Code / Codex CLI / ZCode 的本地 Agent 运行时：Rust 核心 + 单文件 Web UI，
桌面 webview 或浏览器均可承载。模型输出真实流式、工具真实执行文件系统与 shell、权限网关真实拦截。

**这不是聊天界面的壳。** 你说"读一下 Cargo.toml，写个脚本然后跑起来"，它会真的读文件、生成 diff、
写盘、执行命令并把真实输出讲给你听——全过程无任何伪造。

---

## 已验证的能力

用真实模型（Xiaomi MiMo v2.6 Pro，OpenAI 兼容协议）跑通的完整自主循环：

| 步骤 | 工具 | 真实结果 |
|---|---|---|
| 1 | `glob` | 在工作区定位文件 |
| 2 | `list_dir` | 列目录（大小、类型） |
| 3 | `read_file` | 读取带行号的真实内容 |
| 4 | `write_file` | 写盘，生成 `+3 -0` diff 卡片 |
| 5 | `bash` | `python hello.py` → exit 0，`tool.progress` 实时流出 stdout |

中途穿插 `reasoning.delta` 思考流，最后流式输出带 markdown 的总结。

---

## 架构

```
┌─ 桌面壳 (crates/app)  或  浏览器
│   └─ webview 加载 http://127.0.0.1:<random>/?token=<secret>
│
├─ 本地 HTTP 服务 (axum, 127.0.0.1 + 随机端口 + 会话 token)
│   ├─ GET  /api/events   ← SSE：所有状态变化
│   └─ POST /api/*        ← 所有用户动作（X-Parable-Token 必带）
│
├─ Harness 核心 (crates/harness)
│   ├─ agent.rs       单会话 agent 循环、取消、并发控制、状态计量
│   ├─ provider/      anthropic.rs · openai.rs（chat + responses）
│   ├─ tools/         fs · shell · search · web
│   ├─ permission.rs  权限规则引擎 + 挂起式授权 + 审计日志
│   ├─ session.rs     会话/消息/工具卡片持久化
│   └─ server.rs      路由、鉴权、SSE
│
└─ web/index.html     单文件前端（视觉层沿用原 UI，JS 层接真实后端）
```

### 数据流

```
用户输入 → POST /api/chat
        → agent loop:
            装配 system + tools + history
            → provider.stream_chat()  (SSE 解析)
            → text.delta / reasoning.delta  ──SSE──→ 前端逐字渲染
            → 若有 tool_call:
                 权限引擎判定 → (需确认时) permission.request 挂起等待
                 → 执行工具 → tool.call / tool.progress / tool.result ──SSE──→ 工具卡片
                 → tool_result 回填消息历史 → 继续下一轮
        → turn.end
```

---

## 快速开始

```bash
# 构建
cargo build --release

# 启动（打印带 token 的 URL）
./target/release/parable --workspace /path/to/project

# 或无头跑一个任务
./target/release/parable -p "把这个仓库的测试补齐" --workspace .
```

桌面壳：

```bash
cargo run -p app
```

### 配置模型

配置存于 `~/.parable/config.toml`（API Key 在同目录 `credentials` 字段，ACL 已收紧到当前用户）。
三种 API 格式都支持：

| api 字段 | 端点 | 适用 |
|---|---|---|
| `anthropic` | `{base}/v1/messages` | Anthropic 官方、DeepSeek `/anthropic`、Kimi、GLM 兼容端点 |
| `openai` | `{base}/v1/chat/completions` | OpenAI 及一切兼容服务（含 Xiaomi MiMo） |
| `responses` | `{base}/v1/responses` | OpenAI Responses 协议 |

UI 里：设置 → 模型设置 → 添加供应商 → 填 Base URL / API Key → **测试连接**（真实发请求，返回延迟与模型列表）。

---

## 工具集

| 工具 | 风险 | 说明 |
|---|---|---|
| `read_file` | 只读 | 带行号，支持 offset/limit，超限截断并提示续读 |
| `write_file` | 写 | 覆盖写，已存在文件会先生成 diff 预览进权限面板 |
| `edit_file` | 写 | 精确字符串替换，唯一性校验，`replace_all` 可选 |
| `list_dir` | 只读 | 含大小与类型 |
| `glob` | 只读 | `globset` 语义，尊重 `.gitignore` |
| `grep` | 只读 | 正则搜索，`ignore` crate 遍历，输出 `file:line:content` |
| `bash` | shell | 流式 stdout/stderr、超时杀进程组、输出截断保留尾部 |
| `web_fetch` | 网络 | 抓取并剥离 HTML 成文本 |
| `todo_write` | 只读 | 计划面板 |
| `ask_user` | 只读 | 向用户提问并等待 |

---

## 权限模型

| 模式 | 行为 |
|---|---|
| `ask` | 写类 / 破坏性 / 网络 / shell 都请求确认；只读免确认 |
| `ws` | 工作区内读写免确认；区外写入、删除、shell、网络仍确认 |
| `full` | 全部放行，仍记审计日志 |

- 决策 `始终允许` 会写入 `~/.parable/permissions.toml` 并持久命中
- 权限请求带 diff 预览，5 分钟无响应按拒绝处理
- 每次调用记入 `~/.parable/logs/audit-<date>.jsonl`
- 路径越界（`..`、符号链接逃逸）在工具层就被拦截

---

## 与原 spec 的偏离（如实说明）

1. **会话存储用 JSON 文件（`~/.parable/sessions/*.json`），不是 SQLite。**
   理由：`rusqlite` 的 `bundled` 特性在 Windows 上要拉 C 工具链，而本地会话数据量很小。
   模块 API 面与 spec 一致，后续整体替换只动 `session.rs`。

2. **compaction 已实现**：上下文占用超 85% 时自动把较早的历史交给模型做结构化摘要
   （Goal / Done / Files changed / Decisions / Open），保留最近 6 条原文，替换折叠区间。
   摘要为空时无损还原历史。前端会显示「已压缩 N 条早期消息」。

3. **桌面壳目前是"启动服务 + 开系统默认 webview"**，Tauri v2 窗口封装未接。

4. **MCP / Skills / Hooks / 子代理 / 计划模式** 均未实现（P4）。

---

## 测试

```bash
cargo test -p harness
```

12 个单元测试覆盖：路径逃逸拦截、`edit_file` 唯一性校验、diff 计数、SSE 分帧解析（含 CRLF）、
HTML 剥离、grep 匹配、Anthropic 请求体构造（tools + thinking）、compaction 的触发阈值 /
折叠保留 / 空摘要还原。

集成测试用 `tests/mock-llm.mjs`（说真实 Anthropic SSE 协议的 mock），已在开发中验证
「流式文本 → tool_use → tool_result 回填 → 第二轮」全链路。

---

## 目录

```
crates/harness/     核心库 + parable 二进制
  src/provider/     anthropic.rs · openai.rs
  src/tools/        fs.rs · shell.rs · search.rs · web.rs
crates/app/         桌面壳
web/index.html      单文件前端（视觉层保留自原 UI）
web/index.orig.html 原始 UI 备份
tests/mock-llm.mjs  集成测试用 mock LLM
PROMPT.md           本项目最初的构建提示词
docs/ARCHITECTURE.md
```

## 安全

- 服务只监听 `127.0.0.1` + 随机端口，每次启动生成随机会话 token
- 所有 `/api/*`（含 `/api/health`、SSE）必须带 token，缺失或错误返回 401
- API Key 永不下发给前端（只回掩码），只落 `~/.parable/config.toml`
- `bash` 用非交互 shell、超时杀进程组、输出长度上限
- 破坏性操作（删除、覆盖、区外写入）走权限网关
