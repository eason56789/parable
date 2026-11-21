# Parable 修复报告(2026-11-13)

对全仓库(~16k 行)做了 4 路并行审查(后端核心 / 服务器与安全 / 工具与 Provider / 前端),
共发现并修复 **约 100 项** 问题。全部改动见 `CHANGES.patch`,以下为分类摘要。

## P0(崩溃 / 数据丢失 / 可被利用)

| # | 位置 | 问题 | 修复 |
|---|---|---|---|
| 1 | session.rs | 持久化非原子写 + 错误吞没,崩溃/断电即丢整个会话;并发写可交错损坏 | 原子写(临时文件+重命名,Windows 兼容)+ persist 返回 Result 全部上抛记日志;损坏文件自动备份为 `*.corrupt-<ts>` |
| 2 | fs.rs read_file | 长行按字节截断,`String::truncate` 非字符边界直接 panic(中文项目必踩) | 字符边界回退,极端情况 lossy |
| 3 | server.rs | `/` 静态页无鉴权注入 token + 无 Host 校验 → DNS rebinding 可拿 token 后全链路接管(执行任意命令) | 全路由 Host 校验(`127.0.0.1/localhost:<port>`)+ Origin 同源校验 + token 常量时间比较 |
| 4 | permission.rs + tools/mod.rs | ws 模式 bash 免确认(cwd 恒在工作区内);"始终允许"落成 cwd 级规则 = 一次批准永久放行该目录**任意命令**(rm -rf 也免确认);确认面板不显示命令(盲批) | shell/网络/破坏性一律送确认;面板显示完整命令;always 规则改为**精确命令匹配**(`exact: true`) |
| 5 | tools/mod.rs | `schedule_*`/`spawn_agent` 无权限判定 + 定时任务硬编码 `permission_mode:"full"` → 模型可自我提权(30 秒后无人值守 full 执行任意 prompt) | 全部接入权限网关;定时任务/子代理继承会话权限模式 |
| 6 | tools/mod.rs | `authorize_with_preview` 缺 plan_mode 检查 → 计划模式下 write/edit 直接落盘 | 两个 authorize 共用同一前置守卫 |
| 7 | provider/openai.rs | HTTP 错误体按字节切片,中文错误页可 panic | 边界安全截断 |
| 8 | 前端 | 附件链路整体失效:依赖 Electron 私有属性 `File.path`(浏览器恒 undefined),附件永远到不了模型,且无报错 | 新增 `POST /api/attachments` 上传端点(20MB/文件名净化/魔数判图),前端先上传再发 path |
| 9 | agent.rs + provider | compaction 切分点不看 tool_use/tool_result 配对,可把会话打成永久 400 死循环 | 切分点对齐完整消息组(`align_compact_split`) |

## 安全加固(P1)

- **审计覆盖**:审计下沉到 `Registry::execute`,自动放行(ws/full/规则命中)也记录决策来源与耗时;README 声称的 "full 仍记审计" 现在为真
- **凭据防护**:只读工具读取 `~/.parable` 内部(配置/会话/审计)直接拒绝;工作区外读取需确认;config.toml 落盘收紧 ACL(Unix 0600 / Windows icacls)
- **符号链接逃逸**:对"不存在的新文件"逐级向上找最长已存在前缀做 canonicalize 复检,悬空链接直接拒绝(修复前写 `link/newfile.txt` 可越界)
- **inside_workspace**:组件级比较(修 `C:\ws` vs `C:\ws-secret` 误判、`..` 归一化、Windows 大小写、`\\?\` 前缀)
- **SSRF**:web_fetch 拒绝环回/私网/链路本地/云元数据地址,自实现重定向逐跳复检,10MB 下载上限
- **插件盲批**:plugin 工具确认面板显示渲染后的真实命令;plugin_create 展示完整 manifest(防持久 prompt injection 载体)
- **chat permission_mode 白名单**(ask/ws/full,其余 400);computer CDP 端口随机化 + 浏览器指纹校验
- `permissions.toml` 解析失败告警;config 解析失败先备份再回退默认(防覆盖真实配置)

## 逻辑修复(P1)

- **工具执行顺序**:移除"只读全并行重排"(模型发 `bash(建文件)+read_file(读它)` 会读不到);改为按原始顺序、仅相邻连续只读并行
- **turn.end 不发**:出错路径前端永久"运行中" → 统一收尾守卫,所有出口(含 panic,catch_unwind)必发 turn.end 并清理运行槽
- **取消丢内容**:流中取消不再丢弃已流出文本,折叠落库后收尾
- **子代理**:完成判定改用 runtime.is_running(修复把中间文本当最终答案);interrupt 真正取消子 turn;300s 超时不再留孤儿运行槽;seed_history 角色映射修正;Caller 投递改为显式报错;权限模式继承父轮
- **Anthropic thinking 签名**:signature 采集链路接通(`StreamChunk::ReasoningDone`),thinking+tools 第二轮不再 400
- **Provider**:SSE 改字节级缓冲(跨 chunk 的多字节字符不再损坏,工具参数 JSON 不再被截坏后静默空对象);OpenAI 流中断不再丢工具调用、BTreeMap 保序、缺 id 合成;`reasoning_effort`/`max_completion_tokens` 映射;stop_reason 透传;responses 协议错误信息与 `.done` 兜底
- **bash**:超时杀整棵进程树(Windows `taskkill /T`,Unix 进程组)+ 收尾 3 秒超时(修复"超时后永久挂起");非 UTF-8 行 lossy 不丢行;截断保留头+尾(错误摘要在尾部);上限后停发 progress
- **fs**:diff 新旧行号独立游标(修复行号漂移);二进制 NUL 嗅探;空区间与空文件区分;`edit_file` 空串防御;list_dir 500 条上限
- **会话存储**:cache 未命中回退磁盘;解析失败不再静默丢消息;`schedule.json` 损坏备份不清空;due() 竞态消除;Target::Existing 校验防幽灵会话
- compaction:阈值计入 system+tools;结果持久化(不再每轮重复付费重摘要);MAX_STEPS stopReason 显式化
- token 估算补韩文/CJK 扩展B;headless `-p` 先订阅再启动(Lagged 不再提前退出);mask_key 字符边界安全

## 前端修复(35 项,P0×1 + P1×11 + P2×23)

- 附件上传全链路(P0);假统计数字全部接真实字段(`modelMs/toolMs/ttftMs`)
- `tool.progress` 渲染(bash 实时输出);代码块含空行不再断裂(围栏状态机);历史会话 markdown 与实时一致
- 生成中加载指示(spark);权限面板完整渲染(路径/命令/风险色标/diff 预览/300s 倒计时/`permission.resolved` 收尾/重连恢复 `GET /api/pending`)
- 删除会话/任务二次确认;/clear 同步服务端;SSE 重连恢复(seq 去重 + busy 看门狗)
- 模型管理补全(删供应商/删模型/编辑/"获取 API Key"链接/id 去重);boot 恢复服务端状态
- esc() 补引号转义(防属性注入);复制失败真实提示;ask_user 回答回显;计划模式 UI 入口;/status 命令;英文漏翻与错标签;设置弹层恢复浮动卡;欢迎页 spark+免责声明等

## 验证

- `cargo test -p harness`:**68/68 通过**(基线 41,新增 27,含符号链接逃逸/SSE 分帧/diff 行号/截断边界/浏览器指纹/compaction 对齐)
- `cargo build --release`(主程序 + 桌面壳)零警告通过
- 端到端实测(真实运行 release 二进制):
  - 安全冒烟 8/8:带/无 token、恶意 Host 403、恶意 Origin 403、key 掩码、permission_mode 白名单
  - mock LLM 全链路:流式 → tool_use → 工具真实执行 → tool_result 回填 → 第二轮
  - 权限网关:bash 请求挂起(面板含完整命令)→ once 批准真实执行;区外写入工具层拒绝;always=精确命令(相同命令免确认、不同命令仍确认);审计含 auto/requested/execute
  - 附件上传(kind 判定/路径穿越净化)、`/api/pending`、`/api/status`、`/clear`
  - SSE 事件生命周期完整、seq 严格单调
  - 浏览器 UI 实测:欢迎页/侧栏/输入区渲染正常

## 已知限制(如实)

- 子代理不能再嵌套 spawn 子代理(明确报错而非静默失效)
- 会话存储仍为整文件 JSON 重写(长会话 O(n²) IO),持久化已原子化,SQLite 迁移留给后续
- "始终允许"对 shell 是精确命令匹配,不含通配/前缀语义
