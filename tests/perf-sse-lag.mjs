// Reproduces the "stuck on thinking" failure: a slow SSE client falls behind
// the broadcast buffer, events get dropped (including `turn.end`), and the UI
// never clears its busy state.
//
// Strategy: open the SSE stream but never read it (or read very slowly), then
// drive enough turns to overflow the broadcast buffer. With the fix in place
// the server emits `events.lost` instead of silently dropping events.
//
// Usage: node tests/perf-sse-lag.mjs <base-url> <token>

const BASE = process.argv[2] || 'http://127.0.0.1:65310';
const TOKEN = process.argv[3] || 'perftest123';
const H = { 'X-Parable-Token': TOKEN, 'Content-Type': 'application/json' };

// A slow client: connect, then read only a trickle.
const res = await fetch(BASE + '/api/events?token=' + encodeURIComponent(TOKEN), { headers: H });
const reader = res.body.getReader();
let seen = 0, lost = null, turnEnd = 0;
const dec = new TextDecoder();
let buf = '';

(async () => {
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let i;
    while ((i = buf.indexOf('\n\n')) >= 0) {
      const frame = buf.slice(0, i);
      buf = buf.slice(i + 2);
      const line = frame.split('\n').find(l => l.startsWith('data:'));
      if (!line) continue;
      let ev;
      try { ev = JSON.parse(line.slice(5).trim()); } catch { continue; }
      seen++;
      if (ev.type === 'events.lost') lost = ev;
      if (ev.type === 'turn.end') turnEnd++;
    }
    // Throttle hard: one chunk per 250ms.
    await new Promise(r => setTimeout(r, 250));
  }
})().catch(() => {});

// Meanwhile, drive events. Each turn emits a handful; a long reply emits one
// text.delta per token. We ask for many turns to overflow the buffer.
const t0 = Date.now();
let turns = 0;
while (Date.now() - t0 < 20000) {
  try {
    await fetch(BASE + '/api/chat', {
      method: 'POST',
      headers: H,
      body: JSON.stringify({ text: `lag-probe-${turns++}` }),
    }).then(r => r.json());
  } catch (e) { /* ignore */ }
}

console.log(
  `PERF_SSE_LAG turns=${turns}\tevents_read=${seen}\tevents.lost=${lost ? lost.dropped : 'none'}\t` +
  `turn_end_seen=${turnEnd}`
);
try { await reader.cancel(); } catch {}
process.exit(0);
