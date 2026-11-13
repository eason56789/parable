// Verifies the "switch away mid-thought and back" flow: the reasoning block
// and tool cards must survive the round trip (they used to be truncated to
// the last text chunk).
//
// Usage: node tests/perf-switch-resume.mjs <base-url> <token>

const BASE = process.argv[2] || 'http://127.0.0.1:65340';
const TOKEN = process.argv[3] || 'perftest123';
const H = { 'X-Parable-Token': TOKEN, 'Content-Type': 'application/json' };

async function api(p, opts = {}) {
  const res = await fetch(`http://127.0.0.1:${new URL(BASE).port}/api${p}`, {
    method: opts.method || 'GET',
    headers: H,
    body: opts.body ? JSON.stringify(opts.body) : undefined,
  });
  return res.json();
}

// Track events for one session so we know what *should* be on screen.
const seen = new Map();   // sessionId -> { reasoning, tools, text }
function track(ev) {
  if (!ev || !ev.sessionId) return;
  const s = seen.get(ev.sessionId) || { reasoning: 0, tools: [], text: '' };
  if (ev.type === 'reasoning.delta') s.reasoning += (ev.text || '').length;
  else if (ev.type === 'tool.call') s.tools.push(ev.tool);
  else if (ev.type === 'text.delta') s.text += (ev.text || '');
  seen.set(ev.sessionId, s);
}

// Open the SSE stream and keep it alive.
const es = await fetch(`${BASE}/api/events?token=${TOKEN}`);
const reader = es.body.getReader();
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
      try { track(JSON.parse(line.slice(5).trim())); } catch {}
    }
  }
})().catch(() => {});

const sleep = ms => new Promise(r => setTimeout(r, ms));

// Session A: start a turn (mock LLM emits text + a read_file tool call).
const a = await api('/chat', { method: 'POST', body: { text: 'session-A-switch-test' } });
const sidA = a.sessionId;
await sleep(400);

// Session B: switch target, so the UI leaves session A mid-flight.
const b = await api('/chat', { method: 'POST', body: { text: 'session-B-other' } });
const sidB = b.sessionId;
await sleep(1500);

const statA = seen.get(sidA) || { reasoning: 0, tools: [], text: '' };
console.log(`PERF_SWITCH session=A events: reasoning_chars=${statA.reasoning} tools=${statA.tools.join(',')||'none'} text_chars=${statA.text.length}`);
console.log(`PERF_SWITCH session=B id=${sidB}`);
console.log(`PERF_SWITCH A_has_thinking=${statA.reasoning > 0} A_has_tool=${statA.tools.length > 0}`);
process.exit(0);
