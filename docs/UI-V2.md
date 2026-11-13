# UI v2 — 卡死修复、右上角悬浮窗、ZCode 侧栏

本轮改动集中在 `web/index.html`（单文件前端）与 `crates/harness`（事件回放、后台终端、排版配置）。
下面按「为什么改 → 改在哪」记录，方便回溯。

## 1. 三个「经常性」缺陷的根因与修复

### 1.1 整页卡死，只有「工作中」前面的小球还在动

症状的特殊之处在于：**刷新无效，必须新开标签页**。这排除了「后端卡住 / 网络断」，
指向渲染线程被同步长任务占满——刷新会重新渲染同一坨内容，于是再卡一次；
新标签页从欢迎页开始，不渲染那坨内容，所以「恢复」了。

三个叠加因素：

| 位置 | 原实现 | 成本 |
| --- | --- | --- |
| `swrite()` → `blankBoundary()` | 每个 delta 对**整个缓冲** `split('\n')` 逐行扫 | O(n²)：3 万字回答 ≈ 数千万次字符操作 |
| `streamCode()` 前身（`rebuildBlock` → `renderCode`） | 每个 delta 整块重建 `<pre>`，每行一个 `<div class="cl">` | 500 行代码 × 每 token 重建 = 每秒数万 DOM 节点 |
| `iwrite()` → `mk()` | **每个字符一个 `<span class="c">`**，带 `filter:blur()` + `animation:pop` | 3 万字符 = 3 万个同时播放 blur 动画的元素，合成线程直接饿死 |

外加 `follow()` 每个 token 读一次 `chat.scrollHeight`（强制同步布局）。

修复：

- `scanBoundary()`：从上次未判定的行继续扫描，`fence` 状态跨 delta 保留，成本只与新到字符有关。
- `streamCode()`：代码块改成增量渲染——行数增加时追加新行，最后一行原地改写；不再整块重建。
- 动画预算 `ANIM_BUDGET = 900`：单个块最多给 900 个字符加逐字动画，超出后直接拼文本节点；
  `PERF.slow` 可整轮关闭动画。
- `scheduleRebuild()`：列表/表格等非散文块按帧合并，且至少间隔 120ms。
- `follow()`：合并到 `requestAnimationFrame`，一帧最多一次。
- `renderReasoning`/历史渲染：`renderBlock(..., noanim = true)` 默认**不**播逐字动画——
  历史消息不是「刚到的」，给它们排 1s 的 pop 只会让切会话变成几秒白屏。

### 1.2 回答结束后仍显示「思考中」

- 实时路径：`text.delta` 到达时把还开着的思考块 `finishThink()`，正文开始即思考结束。
- 收尾路径：`turn.end` 改用 `finishAllThink(curA)`，收掉**所有** `.th`，而不只是 `curThinkEl`。
- 切换会话重放路径：原来在渲染 `sh.text` 后把 `curThinkEl` 清成 `null`，重放出来的思考块
  从此没有任何代码会收尾 → 头部永久停在「思考中…」。现在保留引用。
- 重放路径同样尊重 `S.lvl === 0 || S.think === false`，不再凭空造空思考块。

### 1.3 回答中途切换会话导致思考过程截断

- `dispatch` 里 `turn.end` 原来**对任何 sessionId** 都 `delete SHADOW[sid]`。切走期间结束的 turn，
  其影子缓冲被清空，回到该会话时只剩存储里已落库的部分 → 看起来被截断。
  现在：活跃会话立即清，非活跃会话标记 `ended` 并保留，打开时与存储做后缀比对去重后再补渲染。
- `openSession` 补渲染前先用 `/sessions/{id}/messages` 里的 assistant 文本做空白归一化的
  `includes` 判断，已经落库的不再画第二遍。
- 后端 `append_message(...).ok()` 吞掉落库错误的老问题仍在，但前端不再「假设一定落库成功」。

### 1.4 顺带修好的事件丢失链路

- `EventHub` 增加 **4096 条环形回放缓冲**（`event.rs`），`publish` 时同步入环。
- `GET /api/events?since=<seq>`：先订阅再取快照，回放 `seq > since` 的事件。
  重连不再必然丢 `turn.end`。
- 客户端 `connectEvents()` 自己接管重连，带上 `since=lastSeq`；`hello` 里新增 `latest`，
  服务端重启（seq 从 1 重来）时自动复位水位。
- 请求的 `since` 比环里最老的还旧时，服务端先发一条 `events.lost`，前端走 `reconcileTail()`。

## 2. 右上角悬浮窗（后台智能体 / 后台终端 / 目标与任务阶段）

- 结构：`#dock` 三枚胶囊（计数 + running 高亮），展开为面板。
- 后台智能体：`GET /api/agents?session=` + `subagent.start|end` 事件；运行中可中断。
- 后台终端：新增 `/api/term/start|list|kill|clear`（`crates/harness/src/term.rs`）。
  启动即返回、输出走 `tool.progress` 增量、`kill` 会真正杀掉子进程（`kill_on_drop`）。
- 目标与阶段：`GET /api/sessions/{id}/goal` + `goal` 事件；任务阶段取**最近一次**
  `todo_write` 的 `display.todos`（与 ZCode 的「只认最新 session 分组」一致）；
  当前阶段由 `turn.start/reasoning.delta/tool.call/text.delta/turn.end` 推导。

## 3. 侧边栏（ZCode 复刻）

- `workspace → task` 二级：项目行可折叠、任务首屏 5 条、「显示更多」每次 +5。
- 视图切换：`项目 / 分组 / 时间线`，滑块 pill 样式；偏好存 `localStorage['parable-sidebar']`。
- `Pinned` 固定在切换条与列表之间；归档单独一段、默认收起。
- `INFLIGHT`（正在跑的会话）永远置顶，层内按更新时间倒序。
- 节点按 id 复用，刷新只改文字与状态，不重播入场动画。

## 4. `/side` 与个性化

- `/side` 进入斜杠命令表，打开（或聚焦）侧边聊天 tab；侧聊走 `/session/new`，不污染主对话。
- 新增设置项：界面字号 `uiSize`、行距 `lineHeight`、代码字号 `codeSize`、消息密度 `density`。
  后端 `Config` 同步新增 `ui_size / line_height / code_size / density` 并做范围收口。
  CSS 变量：`--ui / --lh / --cfs / --dens`。

## 5. 「此功能为演示」类占位清理

- 侧栏「部署」面板原本只写「尚未提供」。现在展示真实工作区状态：
  分支与未提交改动（`/git/status`）、构建入口（`/fs/tree` 里的 Cargo.toml / package.json / Makefile）、
  后台终端数量（`/term/list`）。没有数据时是真实空态，而不是承诺未来会有。
- `sp_deploy_note` 文案删除。

## 6. 验证

- `cargo build -p harness`：通过（仅剩 Cargo.toml 的无关依赖 warning）。
- `cargo test -p harness`：**180 passed / 1 failed**。
  失败项 `tools::fs::tests::readonly_resolution_allows_absolute_outside` 是仓库原有的
  Windows 路径假设用例（在 Linux 上把 `\` 当普通字符），与本轮改动无关。
- 新增测试：`event.rs` 回放/上限 2 个，`term.rs` 2 个，`server.rs` 后台终端 3 个。
- 前端：`node --check` 通过（脚本 26 万字符）。

## 7. 未做 / 边界

- ZCode 的「section 拖拽换序」未实现（需要 pointer 拖拽 + 落位计算，收益低于风险）。
- 侧栏仍是单工作区模型：`工作区` 取会话的 `workspace` 字段，通常只有一个项目行。
- 事件环 4096 条，超长断线仍可能丢中间片段；此时 `events.lost` + `reconcileTail()` 兜底。

## 7. Agent 消息结构化编辑（工具调用 / 工具返回 / 正文）

点 Agent 气泡上的铅笔不再进「就地 textarea」，而是弹窗：

- **工具调用**：每个 `ToolUse` 一行，可改工具名与参数（JSON，保存前校验），
  勾选「标记为错误」写回 `ToolResult.is_error`。
- **工具返回**：对应 `ToolResult.content`，也就是模型真正看到的内容；
  卡片展示同步退化成纯文本（原来的 read/diff/shell 排版已经和改后的正文不符）。
- **正文**：把这段气泡里所有 `Text` part 折叠成一条重写，`Thinking` / `ToolUse` 原样保留。

落库走 `PATCH /api/sessions/{id}/messages/{idx}`，body 增加可选 `tools[]`
（`{id,name,args,result,is_error}`）；`text` 变为可选。旧的「只改正文」调用不改 body 也能用。
后端 `SessionStore::edit_message_parts` 同时改 `parts` 与 `displays`，避免 UI 与服务端两套内容打架。

顺带修了 `edit_message_text` 里 `json!({ key: ... })` 会把 key 当字面量 `"key"` 的问题
（改为 `Map::insert`，否则编辑过的 parts 结构会被写坏）。
## 8. 工具调用记录可增删 + 侧聊也能用工具

编辑弹窗（第 7 节）升级为「整段重建」：

- `PATCH /messages/{idx}` 的 `tools` 变成可选数组；发就算重建，`[]` 表示删光。
  每条带 `step`（归属的 assistant 消息下标），前端新增的默认挂在本气泡最后一步。
- 后端 `edit_message_parts` 按 step 分组重写：assistant 只保留 `Thinking` + 写入
  `ToolUse`，紧跟一条 `ToolResult` user 消息（`display` 同步生成纯文本卡片）；
  原消息里的图片并到最近一条结果消息，不丢图。没出现在 `tools` 里的调用即删除。
- 弹窗每张卡片右上角有垃圾桶（删除），列表下方「+ 添加工具调用」新建空卡片
  （默认 `bash`，参数 `{}`），保存时统一校验 JSON。

侧聊此前只处理 `text.delta`，工具虽然在跑但界面看不到。现在：

- `spOnEvent` 收 `tool.call` / `tool.result`，挂到当前助手消息的 `tools` 数组；
- `spSideSync` 里建「工具卡片区 + 正文区」，卡片复用主对话的 `toolCard`/`fillToolCard`，
  默认折叠、点开看完整返回；正文仍走 `renderBlocks`，增量更新不重建卡片；
- 带工具的一轮服务端会多出 ToolResult 消息，本地条数 ≠ 服务端下标，因此
  `turn.end` 时拉一次 `/messages` 把真实下标钉在 `last.si`，`spSideIdx` 优先用它，
  避免侧聊的重新生成/编辑/删除打错消息。

## 9. 工具卡片中文说明、操作条间距、默认行距

- `toolDesc(name, args)`（前端）按工具名和参数生成 5–15 字中文短语：bash 先认
  `date / ls / cat / rg / cargo build / git status / npm install` 等常见命令，
  其余退化为「执行 xxx 命令」；读/写/编辑取文件名，搜索取 query。
- 卡片头改成 `[图标] 正在运行命令/已运行命令 · 中文说明`（非命令类工具为
  正在运行/已运行），失败时 `命令失败` 标红。完整参数在展开区第一行
  （`tool(args JSON)`）显示，bash 仍用 `$ command` + stdout/stderr；说明本身
  不进模型上下文。
- `tool.call` 事件改为在 `ToolCallDone`（参数收齐）时才发，事件里带完整
  `args`，前端不用等 `tool.result` 才能写说明；`tool.result` 只负责填结果。
- 操作条贴回正文：正文最后一段 `margin-bottom:0`、空段落不渲染、已收尾的
  `.wk` 计时条 `display:none`、`.arow` 上边距 6→3px；`renderBlocks` 先裁掉
  文本尾部空白，避免模型多吐的换行变成空段落。
- 行距默认统一为 1.5（`config.rs::default_line_height`、前端 `lineHeight` 默认值、
  `--lh` 回退值、PATCH 缺省值），设置里仍可 1.2–2.4 调。
- 工具卡头部的状态词（`.st-tx`）与中文说明（`.fi b`）统一为 400 常规字重，避免整行
  看起来过粗；说明仍用主文字色，靠层级而非字重区分。

## 10. Agent 自建工具与插件热生效

- `plugin_create` / `plugin_edit` 的 `tools[]` 除了 shell `command` 模板，还可以直接给
  `code` + `language`（python / powershell / node）。后端把源码写到
  `~/.parable/plugins/<id>/scripts/<tool>.<ext>`，并在清单里记录生成的 `command` 与
  `script` 绝对路径。
- 内联代码工具运行时不再经过 `cmd /C` 拼字符串（Windows 会在路径外层再套引号，
  实测 Python 报 `Invalid argument`），而是由 `run_plugin_tool` 直接用解释器 +
  独立 argv 启动脚本。参数 JSON 放在 `PARABLE_TOOL_ARGS`，另有
  `PARABLE_TOOL_NAME` / `PARABLE_TOOL_WORKSPACE`，避免引号地狱。
- `Registry.plugin_tools` 改为 `RwLock`；`plugin_create/edit/toggle/delete` 成功后
  立即 `reload_plugins()` 并清 `specs_cache`，新工具在当前 turn 的下一步就能被模型
  看到并调用（实测：创建 `text_stats` 后同轮调用返回 `chars=20 words=4 lines=2`）。
- 权限面板对内联代码工具展示真正会执行的源码（diff preview），而不是只有一句路径命令；
  执行超时 120s，输出仍截断 24k。
- 插件页新增删除按钮（确认弹窗 + 二次确认），后端 `DELETE /api/plugins/{id}`；
  每次 `turn.end` 重拉 `/plugins`，agent 本轮新增的斜杠命令下一次输入即可用。
- 工具热生效只覆盖 `tools`；新增 slash command 与 prompt 段在下一次 turn 生效。
## 11. ask_user 问答接管输入框

- 后端 `DisplayKind::Ask` 新增 `answer: Option<String>`；`to_payload` 输出 `answered` 与 `answer`，重开会话可见「你的回答」；超时/中断显示「未回答」。`ask_user` 拒绝空问题，选项去空白/去重并限制 8 个，模型结果明确区分「已回答 / 空回答 / 未回答」。
- 前端不再在工具卡里展开输入框：`question` 事件只保持卡片折叠并标记「等待回答」，问答内容接管底部 composer。
- `.box` 只做纵向高度动画（宽度不变）：textarea、附件、权限/模型/发送等控件 `.5s` 淡出，问答内容 `.5s` 淡入；提交答案后同一曲线反向恢复。统一使用 `cubic-bezier(.42,0,.58,1)`（慢—快—慢），固定 `0.5s`。
- 打开会话恢复未回答的 `ask_user` 时，`openAskSurface(..., instant=true)` 给 box 加 `qsnap`，直接落到问答终态（surface opacity 1、普通控件 opacity 0、高度为目标值），两帧后移除 `qsnap` 并聚焦，避免切换会话时 0.5s 过渡尚未结束造成的问答层与普通控件叠层。切走会话时 `qsReset` 立即清 `qask`/隐藏 surface，不再留下半透明层。
- 选项按钮可直接提交，自定义输入仍保留；`/api/answer` 返回 `answered:false` 时提示问题已结束。`qsReset` 只允许第一次关闭请求设置计时器，避免 `tool.result` 抢先到达时清掉恢复计时、把 composer 卡在 `qrestore` 状态；每题重新启用发送按钮，修复「第一题能答、第二题发送键仍禁用」。

## 12. 切换会话：落库边界与问答恢复

- 服务端在 assistant 步（thinking / tool_use / text）和工具结果批次成功 `append_message` 后发布 `step.persisted`（含 `turnId` 与 `role`）。前端收到后清空该会话的 SHADOW 流事件与在途文本；`tool.result` 不再提前清空，避免 assistant 步还没落库就把思考缓冲丢掉。
- `openSession` 只重放 SHADOW 中确实未落库的尾巴；如果最后一条存储消息已经是 assistant 步，或已经是工具结果批次，就直接复用最后一条 assistant 气泡（补「工作中」计时条），不再 `startTurnUI()` 造一条空白/重复气泡。
- 事件丢失时，`dropPersistedShadow()` 按最后一条 assistant 存储消息的 tool id / thinking 文本做兜底裁剪，防止同一段思考、工具卡在切回时出现两份。
- 存储里的 `ask_user` tool_use 若还没有对应 tool_result，切回会话会从 `args.question/options` 重建折叠工具卡，并重新打开底部问答面板；同一 callId 的重复 `question` 事件直接忽略。
- 已实测：ask_user 等待回答时切走再切回，问题面板恢复且卡片/思考各一份；普通 bash 执行中与完成后切回，思考块和工具卡都不重复、不截断。

## 13. 工具间思考开关与终止按钮清理

- 设置「通用」新增独立开关 `toolThink`（默认 `false`，只写前端 `localStorage`，不进入 `/api/config`）：关闭时实时流只在首次工具调用前显示思考，`turnSawTool=true` 后 `renderReasoning` 直接返回；历史重画按每个 assistant 气泡维护 `aHasTool`，工具之后的 `think` 段跳过。
- 开关补中英文文案与设置搜索关键词（工具/工具调用/tool/think）；旧 localStorage 缺少字段时按 `false` 合并。
- 实测默认关闭时一次 bash 轮次只显示 1 个思考块；打开并刷新重开会话出现 2 个（含工具后的思考），再关闭恢复 1 个且 `toolThink:false`。
- 删除忙碌态输入框左侧独立的 `#stopbtn`（HTML/CSS/JS 全清）；现在忙碌且输入为空时发送键自身显示停止图标/「终止本轮」，忙碌且输入有内容时发送键作为排队/引导，不再出现额外终止键。

## 14. 设置页与聊天悬浮工作台隔离

- 聊天页右上角 `#dock`（后台智能体 / 终端 / 目标任务）不属于设置页；`openSet()` 给它加 `set-hide` 并在关闭设置时移除，避免悬浮条压在「添加供应商」等设置按钮上。
- 实测：打开设置后 `#dock` 的 `display` 为 `none`；返回工作区后恢复 `flex`。

## 15. 缺少模型/API Key 时快速失败

- `/api/chat` 在创建会话之前调用 `Agent::validate_model`；没有可用 provider 或 Key 时直接返回 409，不再先建空会话再在后台起一轮。
- 前端 `run()` 的 catch 在 `turn.start` 尚未到达、`curA` 为空时补一个错误气泡，永久显示具体错误（例如 `provider DeepSeek has no API key`），不再只弹一条会消失的 toast。
- 实测：无 Key 发送返回 409，界面显示红色错误气泡，`busy` 为 false，不再停在「工作中 1 秒」。
