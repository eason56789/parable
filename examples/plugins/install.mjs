#!/usr/bin/env node
/**
 * Parable 插件安装器 —— 把本目录下的插件装进 ~/.parable/plugins。
 *
 * 手写 plugin.json 里的 code 不会像 plugin_create 那样自动物化成脚本文件，
 * 本安装器补齐这一步：把 code 写进 <plugin>/scripts/<tool>.<ext>，并在
 * manifest 里填上 script（绝对路径）与 command（解释器命令，供权限面板展示），
 * 最后在 enabled.json 里登记启用。完全对齐 crates/harness/src/plugin.rs 的
 * write_script() 命名与格式，装出来的插件和 UI 创建的原生一致。
 *
 * 用法：
 *   node install.mjs            # 装全部插件到默认 home（~/.parable）
 *   node install.mjs blender    # 只装指定 id
 *   node install.mjs --home D:\x   # 指定 PARABLE_HOME
 */
import fs from "node:fs";
import path from "node:path";
import os from "node:os";

const argv = process.argv.slice(2);
let home = null;
const targets = [];
for (let i = 0; i < argv.length; i++) {
  if (argv[i] === "--home") home = argv[++i];
  else targets.push(argv[i]);
}
if (!home) home = process.env.PARABLE_HOME || path.join(os.homedir(), ".parable");

const here = path.dirname(new URL(import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1"));
const runners = {
  python: (p) => (process.platform === "win32" ? "python" : "python3") + ` "${p}"`,
  powershell: (p) =>
    (process.platform === "win32" ? "powershell" : "pwsh") +
    ` -NoProfile -ExecutionPolicy Bypass -File "${p}"`,
  node: (p) => `node "${p}"`,
};
const exts = { python: "py", powershell: "ps1", pwsh: "ps1", node: "js", javascript: "js", js: "js" };

function installOne(dir) {
  const id = path.basename(dir);
  if (!/^[a-z][a-z0-9_-]{0,63}$/.test(id) || id === "enabled") {
    return { id, ok: false, msg: "id 非法（需 [a-z][a-z0-9_-]，且不叫 enabled）" };
  }
  const manifestPath = path.join(dir, "plugin.json");
  if (!fs.existsSync(manifestPath)) return { id, ok: false, msg: "没有 plugin.json" };
  const m = JSON.parse(fs.readFileSync(manifestPath, "utf8"));

  const dest = path.join(home, "plugins", id);
  fs.mkdirSync(path.join(dest, "scripts"), { recursive: true });

  let tools = 0;
  for (const t of m.tools || []) {
    const lang = (t.language || "python").toLowerCase();
    if (!t.code) continue;
    const ext = exts[lang];
    if (!ext) return { id, ok: false, msg: `工具 ${t.name} 的 language 非法：${lang}` };
    const sp = path.join(dest, "scripts", `${t.name}.${ext}`);
    // Windows PowerShell 5.1 对无 BOM 的 UTF-8 按 ANSI 解析：中文会乱码并可能
    // 打破字符串字面量。ps1 一律写 UTF-8 BOM（python/node 不需要）。
    if (ext === "ps1" && !t.code.startsWith("\uFEFF")) {
      fs.writeFileSync(sp, "\uFEFF" + t.code);
    } else {
      fs.writeFileSync(sp, t.code);
    }
    t.script = sp;
    t.command = runners[lang](sp);
    tools++;
  }
  fs.writeFileSync(path.join(dest, "plugin.json"), JSON.stringify(m, null, 2));

  // enabled.json 合并登记为启用（PL-05：未登记默认禁用）
  const enabledPath = path.join(home, "plugins", "enabled.json");
  let enabled = {};
  try { enabled = JSON.parse(fs.readFileSync(enabledPath, "utf8")); } catch {}
  enabled[id] = true;
  fs.mkdirSync(path.dirname(enabledPath), { recursive: true });
  fs.writeFileSync(enabledPath, JSON.stringify(enabled, null, 2));

  return { id, ok: true, msg: `已安装 → ${dest}（物化 ${tools} 个脚本，已启用）` };
}

const subdirs = fs
  .readdirSync(here, { withFileTypes: true })
  .filter((e) => e.isDirectory())
  .map((e) => path.join(here, e.name))
  .filter((d) => fs.existsSync(path.join(d, "plugin.json")))
  .filter((d) => targets.length === 0 || targets.includes(path.basename(d)));

if (!subdirs.length) {
  console.log("没有可安装的插件目录（或 id 过滤未命中）");
  process.exit(1);
}
let failed = false;
for (const d of subdirs) {
  const r = installOne(d);
  console.log(`${r.ok ? "✓" : "✗"} ${r.id}: ${r.msg}`);
  if (!r.ok) failed = true;
}
console.log(`\n完成。重启 Parable（或在「设置 → 插件」里切换一次）即可生效。`);
process.exit(failed ? 1 : 0);
