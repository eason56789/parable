# ZCode 使用统计页面源码（提取自 app.asar）

来源：`C:/Users/eason/AppData/Local/Programs/ZCode/resources/app.asar`
ZCode 是 Vite 打包的 Electron 应用，逻辑模块对应 `/out/renderer/assets/` 下的 chunk。

## 文件说明

| 文件 | 原 chunk | 内容 |
|---|---|---|
| `UsageSnapshot.schema.js` | `/out/main/chunk-GMTOLIE6.js` | zod schema，定义快照数据契约（`summary`/`heatmap`/`dailyModelUsage`/`models`/`tools`） |
| `UsagePage.shell.js` | `styles-BhvI6_DD.js` | 页面主体：`z9n`=页面根、`B9n`=概览条、`A9n`=热力图卡片、`U9n`=区间切换 |
| `UsageHeatmap.helpers.js` | `styles-BhvI6_DD.js` | 热力图算法：52 周网格、分位定级、周/累计聚合、月份标签合并 |
| `AppUsageDailyModelTrendChart.js` | `AppUsageDailyModelTrendChart-*.js` | 每日 Token 趋势折线图（recharts LineChart，按模型分线，只取前 6 个模型） |
| `AppUsageModelUsagePieChart.js` | `AppUsageModelUsagePieChart-*.js` | 模型用量环图（内嵌 recharts PieChart，取前 6 个，其余合并为「其他模型」） |
| `usageStatsUiParts.js` | `usageStatsUiParts-*.js` | 共用格式化函数：紧凑数字、百分比、日期、空态组件、分段控件样式 |
| `appUsageChartPalette.js` | `appUsageChartPalette-*.js` | 图表取色：6 个 `--color-usage-chart-N` 循环 |
| `i18n.usage.zh.txt` | `IntlProvider-*.js` | 中文文案（`settings.usage.*`，151 条） |
| `i18n.usage.en.txt` | `IntlProvider-*.js` | 英文文案（151 条，与中文逐条对齐） |

## 怎么重新提取 / 定位原始位置

提取脚本在仓库根：`extract-zcode-usage.js`

```bash
node extract-zcode-usage.js    # 重新生成本目录
```

ZCode 升级后 chunk 的 hash 会变，脚本里按**标记字符串**定位而不是按文件名，
所以通常不用改。如果某个标记找不到，脚本会直接报错并指出是哪个：

| 标记 | 用途 |
|---|---|
| `function A9n(` | 热力图卡片组件起点 |
| `settings.usage.lifetimeTotalTokens\`` | 概览条位置（`B9n` 在它前面） |
| `var Xh=["all","7d","30d"]` | 快照 schema 起点 |
| `var fU=8192` | schema 结束位置 |
| ``"settings.usageTitle":`使用统计` `` | 中文文案块起点 |
| ``"settings.usageTitle":`Usage stats` `` | 英文文案块起点 |
| ``"settings.usageTitle":`使用状況` `` | 日文块起点（用于截断英文） |

> 注意：`styles-BhvI6_DD.js` 名字像 CSS，但 Vite 把共用页面逻辑也放进了这个
> chunk（7.1 MB）。使用统计页面的外壳、热力图、系统健康度都在里面。

## 阅读顺序建议

1. `UsageSnapshot.schema.js` —— 先看数据结构，知道页面能拿到什么
2. `UsageHeatmap.helpers.js` —— 最值得抄的算法
3. `UsagePage.shell.js` —— 页面布局与调用关系
4. `AppUsageDailyModelTrendChart.js` + `AppUsageModelUsagePieChart.js` —— 两张图
5. `usageStatsUiParts.js` —— 格式化细节（万/亿加空格、百分比精度）
6. `i18n.usage.zh.txt` —— 所有文案措辞

## 关键数据契约

```
range: "all" | "7d" | "30d"
generatedAt: number
timeZone: string
source: "agent-db"

summary: {
  totalTokens, inputTokens, outputTokens, reasoningTokens,
  cacheCreationTokens, cacheReadTokens, cacheHitRate,
  totalSessions, totalTurns,
  toolCallCount, toolErrorRate, modelErrorRate,
  avgTimeToFirstTokenMs: number|null, avgTurnDurationMs: number|null,
  activeDays, currentStreakDays, longestSessionMs, longestStreakDays, peakDayTokens,
  favoriteModel: { modelId: string|null, totalTokens, share } | null
}

heatmap: {
  startDate: string|null, endDate: string|null, maxTokens: number,
  weeks: [{ weekIndex, days: (cell|null)[] }]
}
cell: { date, level: 0|1|2|3|4, totalTokens, turnCount, toolCallCount }

dailyModelUsage: [{ date, models: [{ modelId, totalTokens }] }]
models: [{ modelId, totalTokens, inputTokens, outputTokens, requestCount, share }]
tools:  [{ toolName, callCount, errorCount, errorRate, avgDurationMs|null }]
```

## 热力图算法的三个要点

1. **网格固定 52 周 × 7 天**（`u9n=52`、`d9=7`），不是 53。
2. **周/累计模式下按聚合值重新定级**：`C9n(v,max) = min(4, max(1, ceil(v/max*4)))`。
   日模式用服务端给的 `level`，周/累计模式自己重算，且用聚合后的最大值做分母。
   这一条是色阶不至于「全部最高档」的关键。
3. **周/累计模式的着色只落在每列最后 N 格**：`w9n(v,max) = min(7, max(1, ceil(v/max*7)))`，
   不是整列同色——柱高表现累计量。

## 布局顺序（`z9n`）

概览条(B9n) → 热力图(A9n) → 区间选择器(U9n) → 每日趋势图 → 模型环图 → 刷新按钮

## 与 Parable 实现的差异

Parable 没有引入 recharts，图表全部是手写 SVG；数据源是本地
`~/.parable/usage/journal.jsonl`（按轮追加），不是 agent DB。
Parable 额外提供了：Token 构成堆叠条、活跃时段（24h + 星期）、
用量最高的会话列表——这三项 ZCode 没有。
