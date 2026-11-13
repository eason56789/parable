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

桌面壳（Tauri v2，进程内起本地服务，窗口加载固定桌面端口 7789 的 HTTP UI）：

```bash
cargo run -p app

# Windows NSIS 安装包（产物在 target/release/bundle/nsis/）
npx --yes @tauri-apps/cli@2.12.4 build --config crates/app/tauri.conf.json --bundles nsis --ci --no-sign
```

桌面壳首次启动且 `config.toml` 的 `workspace` 为空时，默认使用 `~/.parable/workspace`（自动创建），
避免从安装目录启动时往 `Program Files` 写文件。页面优先读磁盘 `web/index.html`（开发热改），
安装包运行时回退到编译期内置的同一份 HTML，因此不依赖源码目录。

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

核心文件/shell 工具:

| 工具 | 风险 | 说明 |
|---|---|---|
| `read_file` | 只读 | 带行号,支持 offset/limit,超限截断并提示续读;二进制文件有 NUL 嗅探提示 |
| `write_file` | 写 | 覆盖已有文件记为破坏性风险,生成 diff 预览进权限面板;路径锁定工作区 |
| `edit_file` | 写 | 精确字符串替换,唯一性校验,`replace_all` 可选,空 `old_string` 拒绝 |
| `list_dir` | 只读 | 含大小与类型,500 条上限并提示续列 |
| `glob` | 只读 | `globset` 语义,尊重 `.gitignore` |
| `grep` | 只读 | 正则搜索,`ignore` crate 遍历,输出 `file:line:content` |
| `bash` | shell | 流式 stdout/stderr、超时杀整棵进程树(Windows `taskkill /T`,Unix 进程组)、输出截断保留头部+尾部、非 UTF-8 输出按 lossy 解码不丢行 |
| `web_fetch` | 网络 | 抓取并剥离 HTML 成文本;SSRF 防护(拒绝环回/私网/链路本地地址,重定向逐跳复检),10MB 下载上限 |
| `web_search` / `deepseek_search` / `multi_search` | 网络 | 聚合搜索(需在设置里配 key) |
| `todo_write` | 只读 | 计划面板,status 白名单校验 |
| `ask_user` | 只读 | 向用户提问并等待;会话中断时立即唤醒 |

此外还有:`schedule_*`(定时任务)、`spawn_agent`/`agent_list`/`agent_interrupt`(子代理)、`computer_*`(浏览器观察/操作)、`plugin_*`(插件管理)——全部经过同一个权限网关。

---

## 权限模型

| 模式 | 行为 |
|---|---|
| `ask` | 写类 / 破坏性 / 网络 / shell 都请求确认;只读在工作区内免确认,区外读取(含 `~/.parable` 内部)一律确认或拒绝 |
| `ws` | 工作区内新建/编辑文件免确认,覆盖已有文件需确认;shell、网络、破坏性操作一律确认 |
| `full` | 全部放行 |

- 权限请求面板显示工具、路径、风险等级色标、原因(bash 显示**完整命令**)与 diff 预览,300 秒无响应自动拒绝
- 决策 `始终允许` 对 shell 类工具存**精确命令匹配**规则(不是"该目录下任意命令"),其余工具按精确路径,写入 `~/.parable/permissions.toml`
- 每次工具执行(含自动放行)都记入 `~/.parable/logs/audit-<date>.jsonl`,含决策来源(auto/requested)与耗时
- 计划模式(plan mode)在权限引擎层面拦截所有写类/破坏性工具
- 路径越界(`..`、符号链接/目录联接逃逸、不存在新文件的父目录链接逃逸)在工具层就被拦截
- 子代理与定时任务的轮次继承会话的权限模式,不再隐式提权到 full

---

## 与原 spec 的偏离（如实说明）

1. **会话存储用 JSON 文件（`~/.parable/sessions/*.json`），不是 SQLite。**
   理由：`rusqlite` 的 `bundled` 特性在 Windows 上要拉 C 工具链，而本地会话数据量很小。
   写入走"临时文件 + 原子重命名"，损坏文件自动备份为 `*.corrupt-<ts>`，不会覆写丢失。
   模块 API 面与 spec 一致，后续整体替换只动 `session.rs`。

2. **compaction 已实现且持久化**：上下文占用（含 system+tools 固定开销）超阈值时自动把
   较早的历史交给模型做结构化摘要，保留最近 6 条原文；切分点对齐 tool_use/tool_result
   分组，不会把会话切成 provider 拒绝的孤儿 tool_result。压缩结果落盘，重载不回滚。

3. **桌面壳已接 Tauri v2**：`crates/app` 进程内启动 harness 服务，WebviewWindow 加载
   `http://127.0.0.1:<port>/?token=…`，关闭最后一个窗口时中断运行中的 turn。NSIS 安装包
   已构建并实测可启动；安装后无需源码目录即可显示页面（HTML 编译期内置兜底）。

4. **MCP / Skills / Hooks 未实现（P4）；子代理与计划模式已实现**（含权限模式继承与
   完成判定修复），但子代理不能再嵌套 spawn 子代理。

---

## 测试

```bash
cargo test -p harness
```

68 个单元测试覆盖：路径逃逸拦截（含符号链接/悬空链接）、`edit_file` 唯一性校验、diff 行号、
SSE 分帧解析（字节级缓冲，含 CRLF 与跨 chunk 的多字节 UTF-8）、HTML 剥离（自闭合标签/
引号状态/数字实体）、grep 匹配、Anthropic 请求体构造（tools + thinking 签名回放）、
compaction 触发阈值 / 折叠保留 / 空摘要还原 / 切分对齐、bash 输出截断（头尾保留与字符边界）、
二进制嗅探、SSRF 地址校验、浏览器指纹校验等。当前全量：`185 passed`。

集成测试用 `tests/mock-llm.mjs`（说真实 Anthropic SSE 协议的 mock，结构化判断
tool_result），已在开发中验证「流式文本 → tool_use → tool_result 回填 → 第二轮」全链路；
权限网关（bash 精确命令规则、区外写入拒绝）、附件上传、SSE 鉴权（token/Host/Origin）
均经真实服务端到端验证。

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
- 所有路由（含 `/` 静态页与 SSE）必须带 token，缺失或错误返回 401；token 比较为常量时间
- **Host/Origin 双重校验**：Host 头必须是 `127.0.0.1:<port>` 或 `localhost:<port>`，带 Origin 头的
  请求只接受同源——阻断 DNS rebinding 与恶意页面跨站调用本地 API 的完整攻击链
- API Key 永不下发给前端（只回掩码），只落 `~/.parable/config.toml`；落盘后收紧 ACL
  （Unix 0600 / Windows `icacls` 收敛继承），配置损坏时先备份原文件再回退默认
- 附件上传走 `POST /api/attachments`（20MB 上限，文件名净化，落 `~/.parable/uploads/`）
- `bash` 用非交互 shell、超时杀整棵进程树、输出长度上限（保留头尾）
- `web_fetch` 内置 SSRF 防护：环回/私网/链路本地/云元数据地址一律拒绝，重定向逐跳复检
- 破坏性操作（删除、覆盖、区外写入、shell、网络）走权限网关，确认面板带命令/diff/风险色标
- 只读工具读取 `~/.parable` 内部目录（配置、凭据、会话、审计）会被直接拒绝
- 断线重连后前端经 `GET /api/pending` 恢复挂起的权限请求，`permission.resolved` 事件统一收尾
