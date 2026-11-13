# Parable 前端 `index.html` 修改记录

- **修改文件**：`C:\Users\eason\AppData\Local\Temp\opencode\parable-src\web\index.html`
- **修改前备份**：`C:\Users\eason\AppData\Local\Temp\opencode\parable-src\web\index.review-backup.html`
- **压缩包内旧版**：`C:\Users\eason\AppData\Local\Temp\opencode\parable-src\web\index.orig.html`（建议生产排除）
- **原始压缩包**：`C:\Users\eason\Desktop\Parable-前端源码-2026-11-13.zip`
- **改动后行数**：11,317 行（改动前约 10,682 行）
- **日期**：2026-11-14

> 行号为**改动后**当前行号，按符号名定位更可靠（后续再改会漂移）。
> 全部改动集中在单文件，未拆分模块，未引入构建依赖。

---

## 一、验证结果

| 检查项 | 结果 |
|---|---|
| 提取 `<script>` 后 `node --check` | **SYNTAX OK**（397,355 字符，1 个 script 块） |
| 函数名重复扫描 | **0 重复**（288 个函数） |
| 连续重复行 / 复制粘贴残留 | 0 处真实残留 |
| 未定义符号（子代理静态分析） | 0 项 |
| i18n zh/en 键集合一致性 | 473/473，互缺 0（本轮补了 `edit` 键） |
| 三个只读子代理并行审查 | 会话竞态 / 侧聊 / 静态符号，结论已逐条核实 |

---

## 二、P0 — 数据丢失 / 串会话

### P0-1 `restamp()` 配对循环不推进 `.a` 节点（历史遗留缺陷）
- **位置**：`restamp()` :6353
- **问题**：存储里遇到下一条带文本的 user 消息时，`nodes[ni]` 仍停在上一个 `.a` 气泡，`!node.classList.contains('u')` 直接 `break`。结果**第二轮之后的所有消息都拿不到 `data-mi`**，编辑 / 删除 / 重新生成一律提示"这条消息还没落库"。备份文件同样存在，属历史遗留而非本轮引入。
- **修复**：先 `while (ni < nodes.length && !nodes[ni].classList.contains('u')) ni++;` 推进到下一个 `.u` 节点再配对，并补 `ni >= nodes.length` 边界判断。

### P0-2 `openSession()` 中段 `await` 后缺代际校验
- **位置**：`openSession()` :6065（`await restamp()`、`sh.ended` 分支的二次拉取、`else if (sh)` 分支的拉取）
- **问题**：三处 `await` 之后直接写 DOM。用户在请求在途时切走，旧会话的尾巴会 `renderBlocks` 进新会话的最后一个气泡；`curA` 已被 `resetTurn()` 置 null 时还会抛 `TypeError` 并把 JS 错误文本 toast 给用户。
- **修复**：函数入口冻结 `const gen = convGen`，每处 `await` 后补 `if (my !== openTok || gen !== convGen) return;`，并给 `curA` 判空；`focusMsg` 定位同样加守卫。

### P0-3 `newChat()` 不递增 `openTok`
- **位置**：`newChat()` :6718
- **问题**：在途的 `openSession()` 只认 `my !== openTok`，`newChat()` 不动它 → 点了"新对话"后旧请求返回会把界面又切回上一个会话。
- **修复**：`newChat()` 中补 `openTok++`。

### P0-4 侧聊重新生成后操作行闭包不重绑
- **位置**：`spSideSync()` :10150
- **问题**：`spSideRegen()` 用 `st.msgs.splice(i)` 删掉旧消息，新空消息正好落到**同一个 DOM 节点**上。`.arow` 的 `onclick` 闭包仍捕获旧的 `m`/`i`：点「编辑」会把**旧答案** PATCH 到**新消息的下标**上，本地却仍显示新答案；「复制」复制的也是旧答案。
- **修复**：按消息身份 `n.__m !== m` 判断，身份变化即清空 `innerHTML` 并清掉 `dataset.init/tx/editing` 整块重建。

### P0-5 `deleteMsg()` DELETE 不 await 且吞错
- **位置**：`deleteMsg()` :5247
- **问题**：先 `collapseGone(el, restamp)`（330ms 后重算下标），DELETE 不等待、`.catch(() => {})` 吞掉失败。DELETE 延迟或失败时 `restamp()` 拿旧存储配新 DOM，**后续每条 `data-mi` 错一位**，再点删除会删到相邻消息上。确认框期间切走会话还会拿旧下标删新会话。
- **修复**：确认后冻结 `sid/gen` 并复检 `busy`/代际 → `await` DELETE → 失败保留节点、toast + `restamp()`，成功后校验代际再折叠 DOM。

---

## 三、P1 — 竞态 / 输入丢失

| # | 位置 | 问题 → 修复 |
|---|---|---|
| P1-1 | `send()` :5044 | `run()` 提交 `pendingEdit` 的 DELETE 期间 `busy` 仍为 `false`，`send()` 会先清空输入框再被 `runStarting` 挡下，**用户刚打的字凭空消失**。入口加 `runStarting` 守卫 + 提示。 |
| P1-2 | `run()` :4737 | ① POST 失败回滚分支立刻 `awaitingSession=false; pendingSid=null;`（否则 15s 窗口内迟到的 `session.start` 会把界面绑到已丢弃的会话）；② POST 返回的 `sessionId` 与 `conv.id` 不一致时**强制改绑**（POST 响应才是本轮权威）；③ `ignoreSid === conv.id` 时自愈清空；④ 绑定前被会话守卫挡下的事件从 `SHADOW` **补一次重放**（否则开头思考块 / 工具卡 / 正文整段缺失），仅在 `!curA` 时执行避免重复渲染。 |
| P1-3 | `dispatch()` `turn.start` 兜底认领 | ① 要求 `ev.text` **非空**且与 `conv.pendingText` 相等（纯附件首条消息不再被后台重试劫持）；② 认领失败时**仅在 `!awaitingSession` 才标 `ignoreSid`** —— 还在等本端会话时，这条很可能就是被模板改写过文本的本端 turn，标了会把正文和收尾全部丢掉。 |
| P1-4 | `openSession()` catch :6319 | 加载失败时：回滚 `convGen`（`if (convGen === gen+1) convGen--`）、`restorePendingEdit()`（原先 `pendingEdit` 在 fetch 前就置 null，失败后节点成孤儿，界面凭空少一截）、复位 `busy`、`stopTurnProbe()`。`pendingEdit = null` 移到**加载成功后**。 |
| P1-5 | `regenFrom()` :5398 | ① 摘除前记 `anchor`，失败按**原位置** `insertBefore` 插回（原先 `feed.append` 会把旧尾巴排到期间新增的气泡之后，问答顺序颠倒）；② DELETE 成功但 POST 失败时改用 `openSession(sid)` 按服务端状态重放，不塞回已删气泡（否则下次编辑/删除按旧下标改错消息）；③ 轮询推迟到 POST 成功后才 `startTurnProbe()`（DELETE/POST 之间服务端没有 turn，过早轮询会被 `/turn/running=false` 误判收尾）。 |
| P1-6 | `clearConv()` :4968 | 生成中禁止清空（否则 `curA` 指向已删节点，本轮输出既看不见也存不下）；清空后重置 `pendingEdit / expectBubble / awaitingSession / pendingSid` 并 `stopTurnProbe()`。 |
| P1-7 | 侧聊 `spSideRemap()` :10676 | ① 改**正向配对**（原倒序搜索在多段正文/工具结果交错时会命中更靠后的分片）；② 匹配前清掉上一轮过期 `si`，本次没匹配上的消息退回"未知"，不再拿过期下标去编辑/删除。 |
| P1-8 | 侧聊 `spSidePending()` :10832 | 权限/提问处理后 `el.dataset.k = ''` 会与下一个 key（同为空串）命中早退分支，**面板永远清不掉**，剩余按钮还能对已结束的 call 再发一次请求。改为 `delete el.dataset.k`。 |
| P1-9 | 侧聊 `spSideEdit()` :10750 | 加 `dataset.editing` 标记，`spSideSync` 对编辑中的节点 `return`：就地编辑期间 `status` / `permission` 事件触发的重绘不再把 textarea 和保存/取消按钮冲掉。保存/取消/失败三条路径都清标记。 |
| P1-10 | 侧聊 `spSideRegen()` :10713 | ① `await DELETE` 后校验 `SP.tabs.includes(tab)`，标签已关就不再发 `/chat`（否则后端白烧一轮 token、前端无状态跟踪）；② 补 `wkt` 1 秒计时器（原重跑期间"工作中 x 秒"不走）。 |
| P1-11 | 侧聊 `spOnEvent()` :10911 | `error` 且 `fatal` 时停掉 `streaming/cur/wkt`（否则发送键永久停在"停止"、计时器空转）；重绘列表补 `permission.resolved` 与 `error`。 |

---

## 四、P2 — 边界 / 资源 / 静态

### 4.1 功能性

| # | 位置 | 问题 → 修复 |
|---|---|---|
| P2-1 | `loadUsage()` :8416 | 原 `if (ugLoading) return` 会吞掉快速切换 7d/30d 的请求。改为请求令牌 `ugReq`（最后一次请求胜出）+ `body.isConnected` 校验，`ugLoading` 仅在最后一次请求结束时复位。 |
| P2-2 | `memDraw()` :8137 | 原无条件 `ta.focus()`，增删/刷新记忆会把焦点从别处拽走。新增 `memFocus`，只在「刚点新增/编辑」时聚焦。 |
| P2-3 | `boot()` :11267 | ① 全部改 `??`：服务端明确下发的 `false` / `0` / `''` 不再被 `\|\|` 静默改回本地缓存；② `S.side` 的 `sc` 类判断移到**配置合并之后**（原在 `await` 之前，"默认收起"会在 `S.side=true` 时仍生效）；③ `const c = (await api('/config')) \|\| {}` 防空响应。 |
| P2-4 | `uMsg()` :3202 + 复制按钮 :5145 | 用户/助手复制走 `innerText`，会丢 emoji、丢 Markdown 空白与格式。`uMsg` 写 `dataset.raw` 存原文，复制优先取 `dataset.raw`；`restamp()` 回填时为用户消息补写 `raw`（原会把 raw 清掉）。 |
| P2-5 | `editAgent()` :5309 | 保存原为 `api(...).catch(toast)` 后立即 `rebuild` + toast「已保存」——**落库失败界面也显示成功**。改为 `await`，成功才更新 `dataset.raw`/重渲染/toast；失败保留编辑器、复位按钮；快照 `sid` 防保存期间切走。 |
| P2-6 | 附件 objectURL | `URL.createObjectURL` 创建的预览图无人回收，切一次会话漏一批。新增 `OBJ_URLS` 集合统一登记 + `releaseFeedBlobs()`（:5673），在 `openSession` / `clearConv` 清空 feed 前、`newChat` 清空前回收；`newChat` 还回收未发送附件的 URL；输入框手动移除附件时同步从集合删除。 |
| P2-7 | 选择器转义 | `qEditRow()` :3914、`openSession()` :6065、`spSideSync()` 内的 `[data-pid=]` / `[data-id=]` / `[data-call=]` / `[data-mi=]` 拼接选择器，id 含引号/反斜杠时 `querySelector` 直接抛 `SyntaxError`。全部改为 `dataset` 精确比较（`find`）；`mpRow()` :7679 的 `data-mpsel` 属性值补 `esc()`。 |
| P2-8 | `startTurnProbe()` :9075 | 冻结 `sid/gen`；轮询发现会话已切走或轮次已结束时**自己 `stopTurnProbe()`**（原每 3s `return` 永不 `clearInterval`，反复切会话会叠出多个 interval）；合成收尾时补 `expectBubble = false`。 |
| P2-9 | `expectBubble` 闭环 | 置假路径补全：`turn.end`、`error.fatal`、`run()` 失败回滚、`openSession()`、`newChat()`、`clearConv()`、probe 合成收尾。残留 `true` 会让后续后台/重试轮**跳过用户气泡**（只出回答没有提问）。 |
| P2-10 | `confirmBox()` :2086 | 加 `body.modal-open` 锁，全局快捷键（⌘K 切会话、命令面板）在模态期间不响应——否则确认"删除"时切走会话，后续操作落到另一个会话；回车仅在焦点于「确定」时确认（Tab 到取消后回车必须是取消）。 |
| P2-11 | SSE 重连 | `onerror` 不再判断 `document.hidden`（隐藏标签页跳过重连后没有任何路径接回来，切回前台永久断线）；退避定时器存 `connectEvents.timer`；新增 `ensureEvents()` + `visibilitychange`/`focus` 监听，回到前台立刻重建。 |
| P2-12 | `renameChat()` / `deleteChat()` | 原先 UI 先改/先删、请求失败静默吞掉 → 侧栏与刷新后结果对不上。改为等服务端成功才落定，失败回滚标题 / 恢复点击并 toast。 |
| P2-13 | `api()` | 加 `AbortController` 180s 超时（`opts.timeout` 可覆盖），挂死的请求不再让保存按钮、用量页、会话加载永久转圈；超时统一报「请求超时」。 |
| P2-14 | `esc()` :1554 | 补转义单引号 `&#39;`（HTML 属性用单引号包裹的场景原会逃逸）。 |
| P2-15 | `noEmoji()` :1569 | 原正则含 U+2600–27BF / U+2B00–2BFF / 私用区（✓ ✗ ★ ⚠ 与 Nerd Font 图标都会被删），且额外压平行首与连续空格 → **缩进代码、ASCII 表格、粘贴的代码被压平**。改为只删 U+1F000–1FAFF、区域指示符、FE0F、ZWJ，不再做任何空白归一。 |
| P2-16 | 侧聊 `spClose()` :9681 | 关标签时清 `st.wkt`、`st.cur`，若在流式则补发 `/interrupt`（原关掉后面板收不到 `turn.end`，1s 定时器永久空转、后端继续烧 token）。 |
| P2-17 | 侧聊函数守卫 | `spSideSync` / `spSideChrome` / `spSideAtts` / `spSidePending` / `spSideStats` 统一加 `host.id === 'spbody' && tab.id !== SP.active` 早退 + `!host` 判空：异步操作（保存/删除/上传）期间切标签，不再把后台标签内容写进用户正在看的面板。 |
| P2-18 | `regenFrom` / `deleteMsg` 撤回编辑 | 两者入口先 `restorePendingEdit()`：有一条"待发送的编辑"时下标基线会变，不撤回会让两截 DELETE 各截各的。 |
| P2-19 | `restamp()` / `reconcileTail()` / `refreshGoalBadge()` / `onEventsLost()` | `await` 后统一 `gen + sid` 双校验；`onEventsLost` 只同步事件所属会话。 |

### 4.2 静态清理

| # | 位置 | 问题 → 修复 |
|---|---|---|
| S-1 | i18n :1684 / :1853 | `t('edit')` 在 zh/en 字典中**均不存在**，按钮 title 显示英文原文 `edit`。两字典各补 `edit:'编辑'` / `edit:'Edit'`。 |
| S-2 | `ic('close')` :7784 / :7909 | `IC` 表无 `close` 键，回退成「文件」图标。改为 `ic('x')`。 |
| S-3 | 注释残留 | `spSideSync` / `spSideChrome` 上方重复的三行注释、拼接错乱的 `/** ... /** ... */` 已清理。 |
| S-4 | 侧聊 `spSideIdx()` :10653 | `at == null` 守卫此前是死代码（`spSideIdx` 恒返回数字），现配合正向配对让 `si` 语义更准确；保留计数兜底作为 remap 失败时的降级。 |

---

## 五、未处理（P3 / 已知限制）

1. **Markdown 渲染器缺口**（HTML 标签白名单、表格/脚注/数学公式）— 需重写，不在本轮范围。
2. **可访问性**：焦点陷阱、`aria-*` 属性、颜色对比度。
3. **i18n 未全覆盖**：`t('dk_' + 工具名)` 动态键字典未覆盖具体工具名，会显示 `dk_bash` 之类原始 key。
4. **长会话虚拟化**：千条消息全量 DOM，无虚拟列表。
5. **`#navsearch` 死代码**（:6922 引用不存在的 id，有 `if` 守卫，不报错）；`id="mderr"` 在两个模态下重复（查询已作用域化，功能正常，但违反 HTML 规范）。
6. **侧聊缺 turn 探针**：SSE 断连丢 `turn.end` 时侧聊流式状态可能卡住（主对话已有 `/turn/running` 轮询，侧聊未复用）。
7. **侧聊下标兜底**：`spSideIdx()` 在 remap 失败 / 空正文时退回 `forked + 本地条数`，有工具结果时仍可能偏移。正向配对已大幅收窄窗口，彻底解法是给本地消息记 `[si, siEnd)` 区间并在拿到下标前给按钮挂 `.off`。
8. **侧聊停止后迟到的 `turn.end`**：会掐死紧跟着发起的新一轮（需本地轮次计数器匹配，本轮未做）。
9. **`loadServerStatus()` 覆盖本地选择**、`openSession()` 历史附件固定 `[]`、历史思考固定"1 秒"。
10. **`index.orig.html`** 压缩包内旧版与 `index.html` 同目录，建议生产排除。
11. **改动未提交**：目录无 git 版本控制，改动仅存在于临时目录。

---

## 六、改动清单（按符号索引）

```
基础层     esc()  noEmoji()  api()  confirmBox()  uMsg()  copyText()
会话状态   convGen / awaitingSession / pendingSid / pendingEdit / runStarting / OBJ_URLS
           dispatch()  run()  send()  openSession()  newChat()  clearConv()
消息操作   editUser()  restorePendingEdit()  deleteMsg()  regenFrom()
           editAgent()  openAgentEditor()  restamp()  reconcileTail()
后台/连接  startTurnProbe()  stopTurnProbe()  onEventsLost()  ensureEvents()
           connectEvents()  refreshGoalBadge()  loadUsage()  loadServerStatus()
侧聊       spSideSync()  spSideChrome()  spSideAtts()  spSidePending()  spSideSend()
           spSideIdx()  spSideRemap()  spSideRegen()  spSideEdit()  spSideDel()
           spSideStats()  sigStopSide()  spClose()  spOnEvent()  spSideChatInit()
设置/其他  boot()  memDraw()  mpRow()  qEditRow()  releaseFeedBlobs()
```
