// Extract the usage-stats related source out of ZCode's app.asar into
// readable files under ./zcode-usage-ref/
//
// ZCode ships as a Vite bundle: every logical module is a separate chunk under
// /out/renderer/assets/. The usage page's own components are small dedicated
// chunks; the surrounding page shell, the schema and the i18n table live inside
// two very large shared chunks, so those get sliced by marker instead of copied
// whole.
const fs = require('fs');
const path = require('path');

const ASAR = 'C:/Users/eason/AppData/Local/Programs/ZCode/resources/app.asar';
const OUT = path.join(__dirname, 'zcode-usage-ref');

fs.mkdirSync(OUT, { recursive: true });

// ---------------------------------------------------------------- asar reader
const buf = fs.readFileSync(ASAR);
const headerSize = buf.readUInt32LE(12);
const header = JSON.parse(buf.slice(16, 16 + headerSize).toString('utf8'));
const dataStart = 8 + buf.readUInt32LE(4);

const files = [];
(function walk(node, prefix) {
  for (const name of Object.keys(node.files || {})) {
    const f = node.files[name];
    const p = prefix + '/' + name;
    if (f.files) walk(f, p);
    else files.push({ p, size: f.size || 0, offset: Number(f.offset) });
  }
})(header, '');

const read = (suffix) => {
  const f = files.find((x) => x.p.endsWith(suffix) || x.p === suffix);
  if (!f) throw new Error('not found in asar: ' + suffix);
  return buf.slice(dataStart + f.offset, dataStart + f.offset + f.size).toString('utf8');
};

const write = (name, text) => {
  const p = path.join(OUT, name);
  fs.writeFileSync(p, text);
  console.log('  ' + name.padEnd(42) + (text.length / 1024).toFixed(1) + ' KB');
};

// ---------------------------------------------------- 1. dedicated components
console.log('components:');
write('AppUsageDailyModelTrendChart.js', read('AppUsageDailyModelTrendChart-DtsEbOYS.js'));
write('AppUsageModelUsagePieChart.js', read('AppUsageModelUsagePieChart-rTdjcj40.js'));
write('usageStatsUiParts.js', read('usageStatsUiParts-CHAPPnbf.js'));
write('appUsageChartPalette.js', read('appUsageChartPalette-RXzKQcfL.js'));

// ------------------------------------------------- 2. page shell (big chunk)
// The usage page itself lives in styles-*.js (Vite puts shared page code there,
// not just CSS). Slice from the heatmap component to just past the page root.
console.log('\npage shell:');
const styles = read('/out/renderer/assets/styles-BhvI6_DD.js');
const shellStart = styles.indexOf('function A9n(');
const shellMark = styles.indexOf('settings.usage.lifetimeTotalTokens`');
if (shellStart < 0 || shellMark < 0) throw new Error('usage page markers not found');
// A9n = heatmap card, B9n = summary strip, z9n = page root. Take a window that
// covers all three, ending after the refresh button handler.
const shellEnd = styles.indexOf('function L9n(', shellMark);
const shell = styles.slice(shellStart, shellEnd > 0 ? shellEnd : shellMark + 20000);
write('UsagePage.shell.js', shell);

// The heatmap helpers sit immediately before A9n (o9n..A9n). Grab them too.
const helperStart = styles.lastIndexOf('var o9n=', shellStart);
const helpers = styles.slice(helperStart, shellStart);
write('UsageHeatmap.helpers.js', helpers);

// ------------------------------------------------------ 3. schema (main proc)
// The zod schema for the snapshot payload is the authoritative data contract.
console.log('\nschema:');
const main = read('/out/main/chunk-GMTOLIE6.js');
const schemaMark = main.indexOf('var Xh=["all","7d","30d"]');
const schemaEnd = main.indexOf('var fU=8192', schemaMark);
if (schemaMark < 0) throw new Error('schema marker not found');
write('UsageSnapshot.schema.js', main.slice(schemaMark, schemaEnd > 0 ? schemaEnd : schemaMark + 4000));

// ---------------------------------------------------- 4. i18n strings (zh + en)
// Pull every settings.usage* key from the message table. This chunk holds three
// locales; keep zh-CN and en-US, drop the Japanese block.
console.log('\ni18n:');
const intl = read('/out/renderer/assets/IntlProvider-C0vyPcb8.js');
const re = /"settings\.usage([A-Za-z0-9_.]*)":`((?:[^`\\]|\\.)*)`/g;
const zh = [];
const en = [];
// The table holds zh-CN, then en-US, then ja-JP. English starts at
// usageTitle = "Usage stats" and ends where the Japanese block begins
// (usageTitle = 使用状況).
let stage = 0;
let m;
while ((m = re.exec(intl))) {
  const line = 'settings.usage' + m[1] + ' = ' + m[2];
  if (m[1] === 'Title' && m[2] === 'Usage stats') stage = 1;
  if (m[1] === 'Title' && m[2] === '使用状況') stage = 2;
  if (stage === 0) zh.push(line);
  else if (stage === 1) en.push(line);
}
write('i18n.usage.zh.txt', zh.join('\n') + '\n');
write('i18n.usage.en.txt', en.join('\n') + '\n');

// ------------------------------------------------------------- 5. index note
const readme = `# ZCode 使用统计页面源码（提取自 app.asar）

来源：\`C:/Users/eason/AppData/Local/Programs/ZCode/resources/app.asar\`
ZCode 是 Vite 打包的 Electron 应用，逻辑模块对应 \`/out/renderer/assets/\` 下的 chunk。

## 文件说明

| 文件 | 原 chunk | 内容 |
|---|---|---|
| \`UsageSnapshot.schema.js\` | \`/out/main/chunk-GMTOLIE6.js\` | zod schema，定义快照数据契约（\`summary\`/\`heatmap\`/\`dailyModelUsage\`/\`models\`/\`tools\`） |
| \`UsagePage.shell.js\` | \`styles-BhvI6_DD.js\` | 页面主体：\`z9n\`=页面根、\`B9n\`=概览条、\`A9n\`=热力图卡片、\`U9n\`=区间切换 |
| \`UsageHeatmap.helpers.js\` | \`styles-BhvI6_DD.js\` | 热力图算法：52 周网格、分位定级、周/累计聚合、月份标签合并 |
| \`AppUsageDailyModelTrendChart.js\` | \`AppUsageDailyModelTrendChart-*.js\` | 每日 Token 趋势折线图（recharts LineChart，按模型分线，只取前 6 个模型） |
| \`AppUsageModelUsagePieChart.js\` | \`AppUsageModelUsagePieChart-*.js\` | 模型用量环图（内嵌 recharts PieChart，取前 6 个，其余合并为「其他模型」） |
| \`usageStatsUiParts.js\` | \`usageStatsUiParts-*.js\` | 共用格式化函数：紧凑数字、百分比、日期、空态组件、分段控件样式 |
| \`appUsageChartPalette.js\` | \`appUsageChartPalette-*.js\` | 图表取色：6 个 \`--color-usage-chart-N\` 循环 |
| \`i18n.usage.zh.txt\` | \`IntlProvider-*.js\` | 中文文案（\`settings.usage.*\`） |
| \`i18n.usage.en.txt\` | \`IntlProvider-*.js\` | 英文文案 |

## 关键数据契约

\`\`\`
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
\`\`\`

## 热力图算法的三个要点

1. **网格固定 52 周 × 7 天**（\`u9n=52\`、\`d9=7\`），不是 53。
2. **周/累计模式下按聚合值重新定级**：\`C9n(v,max) = min(4, max(1, ceil(v/max*4)))\`。
   日模式用服务端给的 \`level\`，周/累计模式自己重算，且用聚合后的最大值做分母。
   这一条是色阶不至于「全部最高档」的关键。
3. **周/累计模式的着色只落在每列最后 N 格**：\`w9n(v,max) = min(7, max(1, ceil(v/max*7)))\`，
   不是整列同色——柱高表现累计量。

## 布局顺序（\`z9n\`）

概览条(B9n) → 热力图(A9n) → 区间选择器(U9n) → 每日趋势图 → 模型环图 → 刷新按钮

## 与 Parable 实现的差异

Parable 没有引入 recharts，图表全部是手写 SVG；数据源是本地
\`~/.parable/usage/journal.jsonl\`（按轮追加），不是 agent DB。
Parable 额外提供了：Token 构成堆叠条、活跃时段（24h + 星期）、
用量最高的会话列表——这三项 ZCode 没有。
`;
write('README.md', readme);

console.log('\ndone ->', OUT);
