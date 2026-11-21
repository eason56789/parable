// End-to-end check of the Agent Teams feature against a mock LLM.
//
// Verifies the three things that matter:
//   1. A spawned child's result reaches the ROOT's model (the bug where
//      `subagent.result` was published but never consumed).
//   2. The shared task board works over HTTP: create, claim, complete, and
//      a stale revision is refused.
//   3. The roster is scoped to one tree, and team mode gates the board.
//
// Run:  node tests/team-e2e.mjs <port> <token>

import { spawn } from 'node:child_process';
import { createRequire } from 'node:module';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const PORT = Number(process.argv[2] || 8899);
const TOKEN = process.argv[3] || 'testtoken';
const LLM_PORT = PORT + 1;
const HOME = fs.mkdtempSync(path.join(os.tmpdir(), 'parable-team-'));
const BIN = path.resolve('target-team/release/parable.exe');
const require = createRequire(import.meta.url);

const H = { 'X-Parable-Token': TOKEN, 'Content-Type': 'application/json' };

function log(...a) { console.log(...a); }

async function api(p, opts = {}) {
  const res = await fetch(`http://127.0.0.1:${PORT}/api${p}`, {
    method: opts.method || 'GET',
    headers: { ...H, ...(opts.headers || {}) },
    body: opts.body ? JSON.stringify(opts.body) : undefined,
  });
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch (e) { data = { raw: text }; }
  return { status: res.status, data };
}

const sleep = ms => new Promise(r => setTimeout(r, ms));

/** A raw HTTP/1.1 GET, so forbidden headers (Host) are actually sent. */
function rawGet(path, extraHeaders = {}) {
  const net = require('node:net');
  return new Promise((resolve, reject) => {
    const sock = net.connect(PORT, '127.0.0.1', () => {
      const lines = [
        `GET ${path} HTTP/1.1`,
        `Host: ${extraHeaders.Host || '127.0.0.1:' + PORT}`,
        `X-Parable-Token: ${TOKEN}`,
        'Connection: close',
        '', '',
      ];
      sock.write(lines.join('\r\n'));
    });
    let buf = '';
    sock.on('data', d => { buf += d.toString(); });
    sock.on('end', () => {
      const m = buf.match(/^HTTP\/1\.1 (\d+)/);
      resolve(m ? Number(m[1]) : 0);
    });
    sock.on('error', reject);
  });
}

async function waitFor(fn, ms = 30000, every = 250) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    const v = await fn();
    if (v) return v;
    await sleep(every);
  }
  return null;
}

let failures = 0;
function check(name, cond, extra = '') {
  if (cond) log(`  PASS  ${name}`);
  else { failures++; log(`  FAIL  ${name} ${extra}`); }
}

async function main() {
  // --- config: point Parable at the mock LLM, in an isolated data dir
  const cfg = `
name = "team-test"
lang = "zh"
model = "mock/mock-model"
permission_mode = "full"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:${LLM_PORT}/v1"
api = "anthropic"
key = "mock-key"
enabled = true

  [[providers.models]]
  id = "mock-model"
  name = "Mock Model"
  enabled = true
  tools = true
  thinking = false
`;
  fs.mkdirSync(HOME, { recursive: true });
  fs.writeFileSync(path.join(HOME, 'config.toml'), cfg);

  const llm = spawn(process.execPath, ['tests/mock-llm-team.mjs', String(LLM_PORT)], {
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  llm.stderr.on('data', d => process.stderr.write('[llm] ' + d));

  const srv = spawn(BIN, ['--port', String(PORT), '--token', TOKEN], {
    env: { ...process.env, PARABLE_HOME: HOME, RUST_LOG: 'warn' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  srv.stdout.on('data', d => process.stdout.write('[srv] ' + d));
  srv.stderr.on('data', d => process.stderr.write('[srv!] ' + d));

  const up = await waitFor(async () => {
    const r = await api('/health');
    return r.status === 200;
  }, 20000);
  if (!up) {
    log('FATAL: server did not start');
    llm.kill(); srv.kill();
    process.exit(1);
  }
  log(`server up on ${PORT}, data dir ${HOME}`);

  try {
    // --- 1. a turn where the root spawns a child and waits for it
    log('\n[1] spawn -> agent_wait -> result reaches the root model');
    const chat = await api('/chat', {
      method: 'POST',
      body: { text: 'Delegate the research and report back.' },
    });
    check('chat accepted', chat.status === 200, JSON.stringify(chat.data));
    const sid = chat.data.sessionId;

    const done = await waitFor(async () => {
      const r = await api('/sessions/' + sid + '/messages');
      const msgs = r.data.messages || [];
      const texts = JSON.stringify(msgs);
      if (texts.includes('The teammate reported:')) return msgs;
      return null;
    }, 60000);
    check('root produced a final answer', !!done);
    if (done) {
      const all = JSON.stringify(done);
      check("the child's answer reached the root", all.includes('The child found 3 crates'));
      check('the root quoted the teammate', all.includes('The teammate reported:'));
    }

    // --- 2. the roster is scoped to the tree
    log('\n[2] roster');
    const roster = await api('/agents?session=' + sid);
    check('roster is scoped to the session', roster.status === 200);
    const members = roster.data.agents || [];
    check('the spawned child is in the roster', members.length >= 1,
      'got ' + members.length);
    if (members.length) {
      const m = members[0];
      check('the child records its parent', m.parentSession === sid, 'parent=' + m.parentSession);
      check('the child records depth 1', m.depth === 1, 'depth=' + m.depth);
      check('the child records the tree root', m.rootSession === sid, 'root=' + m.rootSession);
    }

    // --- 2b. nesting: the child delegated to a grandchild
    log('\n[2b] nesting');
    const depths = members.map(m => m.depth).sort();
    check('a grandchild exists (the child delegated)', depths.includes(2),
      'depths=' + JSON.stringify(depths));
    const grand = members.find(m => m.depth === 2);
    if (grand) {
      check('the grandchild shares the tree root', grand.rootSession === sid,
        'root=' + grand.rootSession);
      check('the grandchild is a child of the child',
        members.some(m => m.sessionId === grand.parentSession),
        'parent=' + grand.parentSession);
    }
    // The grandchild's answer must have travelled up through the child.
    const nestedMsgs = await api('/sessions/' + sid + '/messages');
    const nestedText = JSON.stringify(nestedMsgs.data.messages || []);
    check("the grandchild's answer reached the root",
      nestedText.includes('The grandchild inspected Cargo.toml'));

    // --- 3. the shared task board over HTTP
    log('\n[3] task board');
    const mode = await api('/team/mode', { method: 'POST', body: { session_id: sid, enabled: true } });
    check('team mode enabled', mode.status === 200, JSON.stringify(mode.data));

    const created = await api('/team/tasks', {
      method: 'POST',
      body: {
        session_id: sid,
        subject: '写文档',
        description: '写 README',
        write_scopes: ['docs'],
      },
    });
    check('task created', created.status === 200, JSON.stringify(created.data));
    const task = created.data.task;
    check('the new task is revision 1', task && task.revision === 1);
    check('the new task is pending', task && task.status === 'pending');
    check('the new task is ready', task && task.ready === true);

    const listed = await api('/team?session=' + sid);
    check('the board lists the task', (listed.data.tasks || []).length === 1);
    check('the board reports team mode on', listed.data.enabled === true);

    const claimed = await api('/team/tasks/' + task.id, {
      method: 'PATCH',
      body: { session_id: sid, expected_revision: 1, action: 'claim' },
    });
    check('claim accepted', claimed.status === 200, JSON.stringify(claimed.data));
    check('claim bumps the revision', claimed.data.task && claimed.data.task.revision === 2);
    check('claim sets the owner', claimed.data.task && !!claimed.data.task.ownerId);

    const stale = await api('/team/tasks/' + task.id, {
      method: 'PATCH',
      body: { session_id: sid, expected_revision: 1, action: 'complete' },
    });
    check('a stale revision is refused', stale.status === 400, 'status=' + stale.status);

    const completed = await api('/team/tasks/' + task.id, {
      method: 'PATCH',
      body: { session_id: sid, expected_revision: 2, action: 'complete' },
    });
    check('complete accepted', completed.status === 200, JSON.stringify(completed.data));
    check('the task is completed', completed.data.task && completed.data.task.status === 'completed');

    // --- 4. a second, unrelated session sees neither the board nor the roster
    log('\n[4] isolation between trees');
    const other = await api('/chat', {
      method: 'POST',
      body: { text: 'Unrelated conversation.' },
    });
    const otherSid = other.data.sessionId;
    const otherTeam = await api('/team?session=' + otherSid);
    check('the other session has no tasks', (otherTeam.data.tasks || []).length === 0);
    check('the other session has no members', (otherTeam.data.members || []).length === 0);

    // --- 5. the team tools are refused when team mode is off
    log('\n[5] team mode gates the tools');
    const off = await api('/team/mode', { method: 'POST', body: { session_id: sid, enabled: false } });
    check('team mode disabled', off.status === 200);
    const afterOff = await api('/team/tasks', {
      method: 'POST',
      body: { session_id: sid, subject: 'x', description: 'y' },
    });
    check('creating a task is refused when the team is off', afterOff.status === 400,
      'status=' + afterOff.status);

    // --- 6. the security guards still hold
    log('\n[6] security');
    const noToken = await fetch(`http://127.0.0.1:${PORT}/api/team?session=${sid}`);
    check('no token -> 401', noToken.status === 401, 'status=' + noToken.status);
    const badOrigin = await fetch(`http://127.0.0.1:${PORT}/api/team?session=${sid}`, {
      headers: { 'X-Parable-Token': TOKEN, Origin: 'http://evil.example' },
    });
    check('a foreign origin -> 403', badOrigin.status === 403, 'status=' + badOrigin.status);
    // Node's fetch drops the forbidden Host header, so the check would be
    // vacuous: send a raw request instead.
    const badHost = await rawGet('/api/team?session=' + sid, { Host: 'evil.example' });
    check('a foreign host -> 403', badHost === 403, 'status=' + badHost);
  } finally {
    llm.kill();
    srv.kill();
    await sleep(300);
  }

  log(`\n${failures === 0 ? 'ALL CHECKS PASSED' : failures + ' CHECK(S) FAILED'}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch(e => { console.error(e); process.exit(1); });
