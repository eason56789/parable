# Parable 内存占用报告

日期：2026-11-13 · 环境：Windows 11 / Rust 1.99.1 / release 构建 / 64 个会话（3.6 MB on disk）

测量方式：`tests/perf-memory.mjs` —— 在各个操作阶段后用 PowerShell 读进程计数器
（`WorkingSet64` = 工作集，`PrivateMemorySize64` = 私有提交量）。

## 一、空闲基线

| 指标 | 数值 |
|---|---|
| Working Set | **12.5 MB** |
| Private Memory | 3.4 MB |
| Threads | 20 |

Rust 运行时 + tokio 线程池 + 已加载的配置/插件，是常驻底噪。

## 二、逐阶段内存增长

| 阶段 | WS | ΔWS | Private | 线程 | 说明 |
|---|---|---|---|---|---|
| 基线（空闲） | 12.5 MB | — | 3.4 MB | 20 | |
| 100 次 `/api/sessions` | 13.6 MB | +1.1 MB | 4.5 MB | 20 | 会话元数据索引 |
| 全量文本搜索（5 个查询） | 21.7 MB | **+9.3 MB** | 13.0 MB | 17 | `text_index`：缓存所有会话的消息正文 |
| 再搜一次（缓存热） | 21.8 MB | +0.1 MB | 13.0 MB | 17 | 命中缓存，无新增分配 |
| 100 次 `/api/tools` | 23.3 MB | +1.6 MB | 13.4 MB | 17 | schema 缓存（`Arc`，共享不复制） |
| 8 个并发 turn | 32.1 MB | +8.8 MB | 18.6 MB | 29 | 每轮的历史消息 + 事件缓冲 |
| 静置 3 秒 | 32.4 MB | +0.3 MB | 18.3 MB | 29 | **不回落**（Rust 不主动归还堆给 OS） |

**峰值 ≈ 32 MB**（空闲 12.5 MB，负载下 +20 MB）。

## 三、主要内存占用者分析

### 1. `text_index`（+8.8 MB）—— 唯一的大头

缓存了每个会话的消息正文（`(下标, 角色, 正文)` 三元组）。磁盘上会话文件共 3.6 MB，
缓存后占 8.8 MB —— **2.4× 膨胀**，来自：
- JSON 里的转义字符在 Rust `String` 里还原
- 每个消息一个 `String` + 一个 `Vec` 条目（per-message 分配开销）
- 正文与 JSON 里的原文重复存了两份（`parts` 里一份，索引里一份）

**对策：LRU 有界缓存**（已实现）

- 上限 `TEXT_CACHE_MAX_BYTES = 4 MB`（原始正文字节数）
- 超限时按 `touched`（最近访问时间）淘汰最久未用的条目
- 保证至少保留 1 条（正在搜的那个会话不会被自己淘汰掉）
- 单元测试 `text_cache_evicts_least_recently_used` 覆盖淘汰顺序

效果：常驻内存从「随归档线性增长」变成「有上限」。当前 3.6 MB 数据未触发淘汰
（测得 +9.3 MB，其中约 4 MB 是索引，其余是 allocator 的碎片与 arena 保留）。

### 2. 会话缓存（`SessionStore::cache`）

`load()` 会把整个 `Session`（含全部消息）放进内存缓存。切换会话时读入，
但从不被淘汰 —— 64 个会话全部读过一次后，常驻内存里就有它们的完整消息。

这是下一个该处理的地方：加 LRU 或「只缓存当前会话」的策略。

### 3. 并发 turn（+8.8 MB / 8 并发 ≈ 1.1 MB 每轮）

每轮要重建完整的历史（`history.clone()` 进 `ChatRequest`）并缓冲事件。
这是模型循环的固有开销，单轮量级正常。

## 四、结论

| 场景 | 内存 |
|---|---|
| 空闲 | 12.5 MB |
| 正常使用（列表 + 搜索 + 偶发对话） | ~23 MB |
| 峰值（8 并发对话） | ~32 MB |

**评价**：对桌面应用来说完全够用。Tauri 窗口的 WebView2 进程本身就要 50-150 MB，
Rust 后端这 30 MB 是次要开销。

**已做的收敛**：
1. `text_index` 加 4 MB LRU 上限（`evict_text_cache`）
2. 工具 schema 改 `Arc` 共享 —— 100 次调用只占 +1.6 MB（此前每次 clone 19 个 JSON schema）

**下一步可选**（按性价比）：
- `SessionStore::cache` 加 LRU（现在是「读过就常驻」）
- 会话存储改 SQLite（消息单独存，避免整文件重写 + 常驻缓存）
- 事件总线在无订阅者时及时释放缓冲

## 五、复现

```bash
./target/release/parable.exe --port 65311 --token memtest123 &
node tests/perf-memory.mjs 65311 memtest123 <pid>
```
