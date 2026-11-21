// In-process HTTP load probe: measures server-side latency without the
// per-request process-spawn cost that dominates curl on Windows.
//
// Usage: node tests/perf-load.mjs <base-url> <token>

const BASE = process.argv[2] || 'http://127.0.0.1:65300';
const TOKEN = process.argv[3] || 'perftest123';
const H = { 'X-Parable-Token': TOKEN };

async function timed(path, n) {
  // warm up
  for (let i = 0; i < 5; i++) await fetch(BASE + path, { headers: H });
  const samples = [];
  const t0 = performance.now();
  for (let i = 0; i < n; i++) {
    const s = performance.now();
    const r = await fetch(BASE + path, { headers: H });
    await r.arrayBuffer();
    samples.push(performance.now() - s);
  }
  const total = performance.now() - t0;
  samples.sort((a, b) => a - b);
  const pct = p => samples[Math.min(samples.length - 1, Math.floor(samples.length * p))];
  console.log(
    `PERF_LOAD ${path}\treq=${n}\ttotal_ms=${total.toFixed(1)}\t` +
    `rps=${(n / (total / 1000)).toFixed(0)}\t` +
    `p50_us=${(pct(0.5) * 1000).toFixed(1)}\t` +
    `p95_us=${(pct(0.95) * 1000).toFixed(1)}\t` +
    `p99_us=${(pct(0.99) * 1000).toFixed(1)}\t` +
    `max_us=${(samples[samples.length - 1] * 1000).toFixed(1)}`
  );
}

async function concurrent(path, n, parallel) {
  const t0 = performance.now();
  let i = 0;
  const workers = Array.from({ length: parallel }, async () => {
    while (i < n) {
      i++;
      const r = await fetch(BASE + path, { headers: H });
      await r.arrayBuffer();
    }
  });
  await Promise.all(workers);
  const total = performance.now() - t0;
  console.log(
    `PERF_LOAD concurrent\tpath=${path}\tparallel=${parallel}\treq=${n}\t` +
    `total_ms=${total.toFixed(1)}\trps=${(n / (total / 1000)).toFixed(0)}`
  );
}

// ---- 1. per-endpoint latency ---------------------------------------------
for (const p of ['/api/status', '/api/tools', '/api/models', '/api/sessions',
                 '/api/agents', '/api/plugins', '/api/schedule', '/api/pending']) {
  await timed(p, 200);
}

// ---- 2. message search (scans every session) ------------------------------
for (const q of ['原神', 'curl', 'zzz_no_such_token']) {
  await timed('/api/search/messages?q=' + encodeURIComponent(q), 30);
}

// ---- 3. concurrency ramp ---------------------------------------------------
for (const par of [1, 8, 32, 64, 128]) {
  await concurrent('/api/status', 400, par);
}

// ---- 4. SSE: time-to-first-event and event throughput ---------------------
async function sseProbe() {
  const ctrl = new AbortController();
  const t0 = performance.now();
  let first = null, bytes = 0, lines = 0;
  try {
    const res = await fetch(BASE + '/api/events', { headers: H, signal: ctrl.signal });
    const reader = res.body.getReader();
    const dec = new TextDecoder();
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (first === null) first = performance.now() - t0;
      bytes += value.length;
      lines += dec.decode(value).split('\n').length;
      if (performance.now() - t0 > 3000) break;
    }
  } catch (e) { /* aborted */ }
  console.log(
    `PERF_SSE handshake\tttfb_ms=${first === null ? 'n/a' : first.toFixed(1)}\t` +
    `bytes=${bytes}\tlines=${lines}`
  );
}
await sseProbe();
