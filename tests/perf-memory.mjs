// Memory probe: watches the harness process RSS while exercising the paths
// that allocate the most (session listing, message search, tool specs,
// concurrent turns) and reports the deltas.
//
// The sampler reads the process counters through a single PowerShell call per
// checkpoint (spawning PowerShell per sample would dominate the measurement).
//
// Usage: node tests/perf-memory.mjs <port> <token> <pid>

const PORT = process.argv[2] || '65307';
const TOKEN = process.argv[3] || 'perftest123';
const PID = Number(process.argv[4]);
const BASE = `http://127.0.0.1:${PORT}`;
const H = { 'X-Parable-Token': TOKEN };

const { execFile } = await import('node:child_process');
const { promisify } = await import('node:util');
const pexec = promisify(execFile);
const sleep = ms => new Promise(r => setTimeout(r, ms));

async function sample() {
  try {
    const { stdout } = await pexec('powershell', [
      '-NoProfile', '-Command',
      `$p = Get-Process -Id ${PID} -ErrorAction SilentlyContinue; if ($p) { "{0} {1} {2}" -f [math]::Round($p.WorkingSet64/1MB,2), [math]::Round($p.PrivateMemorySize64/1MB,2), $p.Threads.Count } else { "0 0 0" }`,
    ]);
    const [ws, priv, threads] = stdout.trim().split(/\s+/).map(Number);
    return { ws, priv, threads };
  } catch {
    return { ws: 0, priv: 0, threads: 0 };
  }
}

const base = await sample();
console.log(`PERF_MEM baseline\tws_mb=${base.ws}\tprivate_mb=${base.priv}\tthreads=${base.threads}`);

function report(label, now) {
  console.log(
    `PERF_MEM ${label}\tws_mb=${now.ws}\tprivate_mb=${now.priv}\tthreads=${now.threads}\t` +
    `delta_ws_mb=${(now.ws - base.ws).toFixed(2)}\tdelta_private_mb=${(now.priv - base.priv).toFixed(2)}`
  );
}

// ---- 1. session listing (meta index) -------------------------------------
for (let i = 0; i < 100; i++) {
  await fetch(BASE + '/api/sessions', { headers: H }).then(r => r.arrayBuffer());
}
await sleep(400);
report('after_100_sessions_list', await sample());

// ---- 2. message search over every session (builds the text index) ---------
for (const q of ['原神', 'curl', 'agent', '性能', 'hello']) {
  await fetch(BASE + '/api/search/messages?q=' + encodeURIComponent(q), { headers: H })
    .then(r => r.arrayBuffer());
}
await sleep(600);
report('after_full_text_search', await sample());

// ---- 3. search again (index warm) -----------------------------------------
for (const q of ['原神', 'curl', 'agent']) {
  await fetch(BASE + '/api/search/messages?q=' + encodeURIComponent(q), { headers: H })
    .then(r => r.arrayBuffer());
}
await sleep(400);
report('after_search_again_warm', await sample());

// ---- 4. tool specs --------------------------------------------------------
for (let i = 0; i < 100; i++) {
  await fetch(BASE + '/api/tools', { headers: H }).then(r => r.arrayBuffer());
}
await sleep(400);
report('after_100_tool_specs', await sample());

// ---- 5. concurrent turns (mock LLM) ---------------------------------------
await Promise.all(
  Array.from({ length: 8 }, (_, i) =>
    fetch(BASE + '/api/chat', {
      method: 'POST',
      headers: { ...H, 'Content-Type': 'application/json' },
      body: JSON.stringify({ text: `mem-probe-${i}` }),
    }).then(r => r.json())
  )
);
await sleep(900);
report('after_8_concurrent_turns', await sample());

// ---- 6. settle: does it shrink once the load subsides? ---------------------
await sleep(3000);
report('after_settle', await sample());
