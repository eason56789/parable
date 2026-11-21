// Mock Anthropic Messages API for the Agent Teams integration test.
//
// The scenario under test is the one that used to be broken: a root agent
// spawns a child, then calls `agent_wait` to collect the child's result. The
// child's answer must reach the root's model as a tool_result.
//
// Root flow (three turns):
//   1. no tool_result yet          -> spawn_agent
//   2. spawn result, no wait result-> agent_wait with the id from the result
//   3. has the wait result         -> final answer quoting the child's answer
//
// The child is recognised by its task marker: it answers once and stops.
//
// Run:  node tests/mock-llm-team.mjs 8901

import http from 'node:http';
import fs from 'node:fs';

const PORT = Number(process.argv[2] || 8901);
const CHILD_ANSWER = 'The child found 3 crates in the workspace.';
const CHILD_MARKER = 'CHILD_TASK_MARKER';
// The child's own task asks it to delegate again — that is the nesting case.
const GRANDCHILD_MARKER = 'GRANDCHILD_TASK_MARKER';
const GRANDCHILD_ANSWER = 'The grandchild inspected Cargo.toml.';

function sse(res, obj) {
  res.write(`event: ${obj.type || 'message'}\n`);
  res.write(`data: ${JSON.stringify(obj)}\n\n`);
}

function split(s, n = 12) {
  const out = [];
  for (let i = 0; i < s.length; i += n) out.push(s.slice(i, i + n));
  return out;
}

function emitText(res, index, text) {
  sse(res, {
    type: 'content_block_start',
    index,
    content_block: { type: 'text', text: '' },
  });
  for (const chunk of split(text)) {
    sse(res, { type: 'content_block_delta', index, delta: { type: 'text_delta', text: chunk } });
  }
  sse(res, { type: 'content_block_stop', index });
}

function emitToolUse(res, index, id, name, input) {
  sse(res, {
    type: 'content_block_start',
    index,
    content_block: { type: 'tool_use', id, name, input: {} },
  });
  const args = JSON.stringify(input);
  for (const chunk of split(args)) {
    sse(res, { type: 'content_block_delta', index, delta: { type: 'input_json_delta', partial_json: chunk } });
  }
  sse(res, { type: 'content_block_stop', index });
}

/** The tool names the model has already called, from the request history. */
function calledTools(messages) {
  const names = [];
  for (const m of messages) {
    const content = m.content;
    if (!Array.isArray(content)) continue;
    for (const block of content) {
      if (block && block.type === 'tool_use') names.push(block.name);
    }
  }
  return names;
}

/** Pull the spawned child id out of the spawn_agent tool result. */
function spawnedId(messages) {
  const text = JSON.stringify(messages);
  const m = text.match(/sa_[0-9a-f]{10}/);
  return m ? m[0] : null;
}

/** The child runs in its own session, whose first user message is its task. */
function isChildTurn(messages) {
  const first = messages[0];
  if (!first || first.role !== 'user') return false;
  const text = JSON.stringify(first.content || '');
  return text.includes(CHILD_MARKER);
}

const server = http.createServer((req, res) => {
  if (req.method !== 'POST' || !req.url.endsWith('/v1/messages')) {
    res.writeHead(404).end('not found');
    return;
  }
  let body = '';
  req.on('data', (c) => (body += c));
  req.on('end', () => {
    let payload = {};
    try { payload = JSON.parse(body); } catch (e) {}

    const messages = payload.messages || [];
    if (process.env.MOCK_LOG) {
      const names = [];
      for (const m of messages) {
        if (Array.isArray(m.content)) for (const b of m.content) if (b && b.type === 'tool_use') names.push(b.name);
      }
      fs.appendFileSync(process.env.MOCK_LOG,
        'CALL msgs=' + messages.length + ' tools=' + JSON.stringify(names) +
        ' first=' + JSON.stringify(messages[0] && messages[0].content).slice(0, 100) + String.fromCharCode(10));
    }
    const tools = calledTools(messages);
    const hasResult = JSON.stringify(messages).includes('tool_result');
    // The child runs in its own session: its FIRST user message is the task.
    // (Checking every message would misfire — the root's spawn_agent args
    // carry the marker too.)
    const isChild = isChildTurn(messages);

    res.writeHead(200, {
      'content-type': 'text/event-stream',
      'cache-control': 'no-cache',
      connection: 'keep-alive',
    });

    sse(res, {
      type: 'message_start',
      message: {
        id: 'msg_mock', type: 'message', role: 'assistant',
        model: payload.model || 'mock-model',
        usage: { input_tokens: 42, output_tokens: 0 },
      },
    });

    let stopReason = 'end_turn';
    const isGrandchild = isChildTurn(messages) && JSON.stringify(messages[0].content).includes(GRANDCHILD_MARKER);
    if (isGrandchild) {
      // The deepest agent just answers.
      emitText(res, 0, GRANDCHILD_ANSWER);
    } else if (isChild) {
      // The child delegates to a grandchild, then waits for it.
      if (!tools.includes('spawn_agent')) {
        emitText(res, 0, 'Delegating to a sub-teammate.');
        emitToolUse(res, 1, 'toolu_gspawn1', 'spawn_agent', {
          label: 'inspector',
          task: GRANDCHILD_MARKER + ': inspect the workspace manifest',
        });
        stopReason = 'tool_use';
      } else if (!tools.includes('agent_wait')) {
        const m = JSON.stringify(messages).match(/sa_[0-9a-f]{10}/g);
        const id = m && m.length ? m[m.length - 1] : 'missing';
        emitText(res, 0, 'Waiting for the sub-teammate.');
        emitToolUse(res, 1, 'toolu_gwait1', 'agent_wait', { agent: id, timeout_seconds: 60 });
        stopReason = 'tool_use';
      } else {
        const found = JSON.stringify(messages).includes(GRANDCHILD_ANSWER);
        emitText(res, 0, found
          ? 'My sub-teammate said: ' + GRANDCHILD_ANSWER
          : 'My sub-teammate finished but its answer did not arrive.');
      }
    } else if (!tools.includes('spawn_agent')) {
      emitText(res, 0, 'Delegating the research to a teammate.');
      emitToolUse(res, 1, 'toolu_spawn1', 'spawn_agent', {
        label: 'researcher',
        task: CHILD_MARKER + ': count the crates in the workspace, then delegate a manifest inspection to a sub-teammate',
      });
      stopReason = 'tool_use';
    } else if (!tools.includes('agent_wait')) {
      const id = spawnedId(messages);
      emitText(res, 0, 'Waiting for the teammate to finish.');
      emitToolUse(res, 1, 'toolu_wait1', 'agent_wait', { agent: id || 'missing', timeout_seconds: 60 });
      stopReason = 'tool_use';
    } else {
      // The child may answer directly, or relay a grandchild's answer.
      const found = JSON.stringify(messages).includes(CHILD_ANSWER)
        || JSON.stringify(messages).includes(GRANDCHILD_ANSWER);
      emitText(res, 0, found
        ? 'The teammate reported: ' + CHILD_ANSWER
        : 'The teammate finished but its answer did not arrive.');
    }

    sse(res, {
      type: 'message_delta',
      delta: { stop_reason: stopReason, stop_sequence: null },
      usage: { output_tokens: 21 },
    });
    sse(res, { type: 'message_stop' });
    res.end();
  });
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`mock anthropic llm (team) on http://127.0.0.1:${PORT}/v1/messages`);
});
