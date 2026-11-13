// Concurrent-turn probe: fires N chat requests at once and measures how the
// harness behaves under parallel sessions (each turn does a tool call, so the
// store, permission broker and event hub all get exercised).
//
// Usage: node tests/perf-concurrent-turns.mjs <base-url> <token> [parallel]

const BASE = process.argv[2] || 'http://127.0.0.1:65306';
const TOKEN = process.argv[3] || 'perftest123';
const PAR = Number(process.argv[4] || 8);
const H = { 'X-Parable-Token': TOKEN, 'Content-Type': 'application/json' };
async function oneTurn(i) {
  const t0 = performance.now();
  const res = await fetch(BASE + '/api/chat', {
    method: 'POST',
    headers: H,
    body: JSON.stringify({ text: `concurrent-probe-${i}` }),
  });
  const body = await res.json();
  const t1 = performance.now();
  return { status: res.status, ms: t1 - t0, session: body.sessionId };
}

async function main(par) {
  // warm up
  await oneTurn('warm');
  const t0 = performance.now();
  const results = [];
  for (let i = 0; i < par; i++) results.push(oneTurn(i));
  const all = await Promise.all(results);
  const total = performance.now() - t0;
  const times = all.map(r => r.ms).sort((a, b) => a - b);
  const ok = all.filter(r => r.status === 200).length;
  console.log(
    `PERF_TURNS parallel=${par}\tok=${ok}/${par}\ttotal_ms=${total.toFixed(1)}\t` +
    `rps=${(par / (total / 1000)).toFixed(1)}\t` +
    `p50_ms=${times[Math.floor(times.length / 2)].toFixed(1)}\t` +
    `max_ms=${times[times.length - 1].toFixed(1)}`
  );
}

for (const par of [1, 4, 8, 16]) {
  await main(par);
}
