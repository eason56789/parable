// Streaming performance probe: drives a real turn through the harness against
// the mock LLM and measures time-to-first-token, event throughput and the
// end-to-end turn latency.
//
// Usage: node tests/perf-stream.mjs <base-url> <token>

const BASE = process.argv[2] || 'http://127.0.0.1:65305';
const TOKEN = process.argv[3] || 'perftest123';
const H = {
  'X-Parable-Token': TOKEN,
  'Content-Type': 'application/json',
};

async function runTurn(label) {
  const sse = await fetch(BASE + '/api/events', { headers: H });
  const reader = sse.body.getReader();
  const dec = new TextDecoder();

  let t0 = null, ttft = null, events = 0, bytes = 0, turnEnd = null, firstDelta = null;
  const types = {};
  let buf = '';

  const pump = (async () => {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (t0 === null) t0 = performance.now();
      bytes += value.length;
      buf += dec.decode(value, { stream: true });
      let idx;
      while ((idx = buf.indexOf('\n\n')) >= 0) {
        const frame = buf.slice(0, idx);
        buf = buf.slice(idx + 2);
        if (!frame.includes('data:')) continue;
        const dataLine = frame.split('\n').find(l => l.startsWith('data:'));
        if (!dataLine) continue;
        let obj;
        try { obj = JSON.parse(dataLine.slice(5).trim()); } catch { continue; }
        events++;
        types[obj.type] = (types[obj.type] || 0) + 1;
        if (obj.type === 'text.delta' || obj.type === 'delta' || obj.type === 'text') {
          if (firstDelta === null) firstDelta = performance.now();
        }
        if (obj.type === 'turn.end') turnEnd = performance.now();
      }
    }
  })();

  // Post the chat request; the SSE stream carries the events.
  const tStart = performance.now();
  const res = await fetch(BASE + '/api/chat', {
    method: 'POST',
    headers: H,
    body: JSON.stringify({ text: label }),
  });
  const chatStatus = res.status;

  // The stream stays open after turn.end (it is a live event channel), so
  // stop as soon as the turn is over rather than waiting for EOF.
  const deadline = Date.now() + 20000;
  while (turnEnd === null && Date.now() < deadline) {
    await new Promise(r => setTimeout(r, 20));
  }
  try { await reader.cancel(); } catch {}
  await pump.catch(() => {});
  const total = (turnEnd ?? performance.now()) - tStart;
  console.log(
    `PERF_STREAM ${label}\tchat_status=${chatStatus}\tevents=${events}\tbytes=${bytes}\t` +
    `ttfb_ms=${firstDelta === null ? 'n/a' : (firstDelta - tStart).toFixed(1)}\t` +
    `turn_end_ms=${total.toFixed(1)}\t` +
    `events_per_s=${(events / (total / 1000)).toFixed(0)}`
  );
  console.log('   types: ' + Object.entries(types).map(([k, v]) => `${k}=${v}`).join(' '));
}

for (let i = 1; i <= 3; i++) {
  await runTurn('perf-stream-probe-' + i);
}
