# Parable 性能测试报告

日期：2026-11-13 · 环境：Windows 11 / Rust 1.99.1 / release 构建 / 64 个会话（3.4 MB）

## 测量方法

| 层 | 工具 | 说明 |
|---|---|---|
| Rust 微基准 | `crates/harness/src/perf.rs` | 稳定版手写计时器（不依赖 nightly/criterion），`cargo test -p harness --lib perf -- --nocapture` |
| HTTP 端点 | `tests/perf-load.mjs` | 单进程 in-process 客户端，排除 curl 进程启动开销（Windows 上 ~20 ms/请求） |
| 并发 turn | `tests/perf-concurrent-turns.mjs` | 真实 agent 循环（走 mock LLM，含工具调用 + 第二轮） |
| 流式 | `tests/perf-stream.mjs` | SSE 首 token / 事件吞吐 / 整轮耗时 |

`tests/perf-http.sh` 是 curl 版本，仅作对照（数字含进程启动开销，不代表服务端）。

## 一、修复前的瓶颈

| 端点 | 修复前 p50 | 根因 |
|---|---|---|
| `/api/sessions` | **7138 µs** | `SessionStore::list()` 每次请求读盘 + 解析**全部 64 个会话的完整 JSON**（含全部消息，3.4 MB），却只取 `meta` |
| `/api/search/messages` | **10125 µs** | 同上（`search_messages` 依赖 `list()`），且每次搜索都重新解析全部消息正文 |
| `Registry::specs()` | **~1000 µs / 次** | 每次调用重建 19 个 JSON schema；agent 在 **step 循环内**每步调用一次 → 100 步的 turn 浪费 ~100 ms |

## 二、修复

1. **会话元数据索引**（`SessionStore::meta_index`）
   - `list()` 改走 `summary_for()`：只解析 JSON 头部（`messages` 键之前的 meta 字段），不载入消息数组
   - 条目按 `(mtime, size)` 判定新鲜度，并加 **750 ms TTL**（Windows 上一次 `stat` 不便宜，64 个会话约 1 ms）
   - 新增 `parse_summary_head()`：截断到 `"messages"` 前再补 `}` 反序列化为 `SessionSummary`
2. **搜索文本索引**（`SessionStore::text_index`）
   - 每个会话的「(消息下标, 角色, 正文)」按文件 stamp 缓存，重复搜索只解析变动过的文件
3. **工具 schema 缓存**（`tools::specs_cache`）
   - 进程级缓存，key = `(search_pref, key 是否存在, plugin 数量)`
   - 返回 `Arc<Vec<ToolSpec>>`，消除 step 循环里的深拷贝；`ChatRequest.tools` 一并改为 `Arc`

## 三、修复后结果

### HTTP 端点（p50 / p95，单位 µs）

| 端点 | 修复前 p50 | 修复后 p50 | 提升 | 修复后 p95 |
|---|---|---|---|---|
| `/api/sessions` | 7138 | **477** | **15.0×** | 667 |
| `/api/search/messages` | 10125 | **2147** | **4.7×** | 2717 |
| `/api/tools` | — | 1164 | — | 1681 |
| `/api/status` | — | 301 | — | 593 |
| `/api/pending` | — | 170 | — | 262 |
| `/api/schedule` | — | 174 | — | 239 |
| `/api/agents` | — | 183 | — | 259 |
| `/api/models` | — | 236 | — | 393 |
| `/api/plugins` | — | 752 | — | 958 |

### 并发（`/api/status`，400 请求）

| 并发度 | 1 | 8 | 32 | 64 | 128 |
|---|---|---|---|---|---|
| rps | 5489 | **7919** | 5796 | 5586 | 4164 |

### 微基准（每操作）

| 项目 | 数值 | 说明 |
|---|---|---|
| `tool_specs_cached` | **0.37 µs** | 缓存前 ~1000 µs（**2700×**） |
| `token_estimate` | 1.31 µs | 每轮估算 |
| `neutral_parse` | 2.89 µs | 历史重放 |
| `search_excerpt_cjk` | 8.0 µs | CJK 片段截取 |
| `persist_atomic_write` | 273–321 µs | 1→200 条消息，增长平缓 |
| `persist_growth_curve` | 290→350 µs | 10→400 条消息 |
| `serialize_50msg` | 1694 µs | 50 条消息 JSON 序列化（**最大剩余热点**） |
| `search_scan_40x50msg` | 10028 µs | 全量扫描 2000 条消息（**次大热点**） |

### 流式（mock LLM，含工具调用 + 第二轮）

| 指标 | 数值 |
|---|---|
| 首 token (TTFB) | **4.6 – 7.6 ms** |
| 整轮（turn.end） | **8.4 – 12.0 ms** |
| 事件吞吐 | **1582 – 2264 events/s** |
| 事件构成 | hello / session.start / turn.start / status×3 / text.delta×9 / tool.call / tool.result / text.done / turn.end |

### 并发 turn（真实 agent 循环）

| 并发 | 成功 | 总耗时 | rps | p50 | max |
|---|---|---|---|---|---|
| 1 | 1/1 | 2.5 ms | 404 | 2.5 ms | 2.5 ms |
| 4 | 4/4 | 6.9 ms | 578 | 5.3 ms | 5.9 ms |
| 8 | 8/8 | 11.8 ms | 681 | 9.8 ms | 10.5 ms |
| 16 | 16/16 | 27.6 ms | 579 | 13.8 ms | 25.6 ms |

## 四、结论

**已解决**：会话列表和消息搜索从「每次请求重读整个存储」降到「内存索引 + 750 ms TTL」，分别快 15× 和 4.7×；工具 schema 缓存消除 100 步 turn 的 ~100 ms 浪费。

**剩余热点（未动，按性价比排序）**：

1. **会话落盘是整文件重写**（`serialize_50msg` = 1.7 ms）。每追加一条消息都要序列化整个会话并原子写盘。代价随历史线性增长（实测：20 条消息时每次追加 707 µs，100 条时 2094 µs）。成本分解：`Value` 树序列化占 92%（100 条消息 3373 µs），会话克隆只占 8%（268 µs）—— 所以换紧凑 JSON 格式没用（只快 8%），真正的解法是**消息按行追加（JSONL）或换 SQLite**。当前规模下可接受：一轮对话的落盘总耗时相对模型推理时间可忽略。
2. **全量搜索仍是 O(全部消息)**（`search_scan_40x50msg` = 10 ms）。现在有缓存，重复搜索很快，但首次搜索或文件变动后仍要扫全部。→ 若会话量继续增长，应加倒排索引。
3. **`/api/tools` 1.16 ms** 是响应体序列化开销（32 个工具的 schema ≈ 15 KB），前端每次调用都重新拉。→ 可加 ETag / 客户端缓存。

**规模上限**：`/api/sessions` 现在分页（默认 200 条）。实测 600 个会话时列表 41 ms、`limit=50` 时 2.8 ms，侧栏渲染流畅（43 MB / CPU 0.2 s）。**上千个会话时若不分页，侧栏的 DOM 构建要数秒，页面看起来就是卡死** —— 这是本次排查中发现的第二个真实卡死成因。

**无回归**：149 个单元测试全绿（含 6 个卡死回归测试）；`/api/sessions`、`/api/search/messages`、`/api/tools`、会话消息读取实测功能正常。

## 五、复现

```bash
# 微基准
cargo test -p harness --lib perf -- --nocapture --test-threads=1

# HTTP 端点（需先启动 release 服务）
node tests/mock-llm.mjs 8899 &
./target/release/parable.exe --port 65305 --token perftest123 &
node tests/perf-load.mjs  http://127.0.0.1:65305 perftest123

# 流式 + 并发 turn（需把 model 切到 mock/mock-model）
node tests/perf-stream.mjs          http://127.0.0.1:65306 perftest123
node tests/perf-concurrent-turns.mjs http://127.0.0.1:65306 perftest123
```

> ⚠️ 探针会创建会话（一次 chat POST = 一个新会话）。**务必用 `tests/perf-run.sh`**：
> 它把 `PARABLE_HOME` 指向一次性临时目录，跑完即删，不会污染真实的 `~/.parable`。
> 直接手跑上面的命令会往用户会话列表里灌测试会话。
>
> ```bash
> bash tests/perf-run.sh tests/perf-load.mjs http://127.0.0.1:65400 perftest123
> ```
