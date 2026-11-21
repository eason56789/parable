// Mock Anthropic Messages API for integration testing the harness agent loop.
// Speaks the real SSE wire format so the provider parser is exercised for real.
//
// Behaviour:
//   call #1 (no tool_result in history) -> streams text, then a read_file tool_use
//   call #2 (history has tool_result)   -> streams a final answer
//
// Run:  node tests/mock-llm.mjs 8899

import http from 'node:http';

const PORT = Number(process.argv[2] || 8899);
// 每段之间的延迟（毫秒）。设大一点可以模拟慢模型，用来测「切走再切回」。
const DELAY = Number(process.argv[3] || 0);

function sse(res, obj) {
  const name = obj.type || 'message';
  res.write(`event: ${name}\n`);
  res.write(`data: ${JSON.stringify(obj)}\n\n`);
}

const sleep = ms => new Promise(r => setTimeout(r, ms));

const server = http.createServer((req, res) => {
  if (req.method !== 'POST' || !req.url.endsWith('/v1/messages')) {
    res.writeHead(404).end('not found');
    return;
  }
  let body = '';
  req.on('data', c => (body += c));
  req.on('end', async () => {
    let payload = {};
    try { payload = JSON.parse(body); } catch (e) {}

    const messages = payload.messages || [];
    // Does the history already carry a tool result?
    const hasResult = JSON.stringify(messages).includes('tool_result');

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

    const emitText = async (index, text) => {
      sse(res, {
        type: 'content_block_start', index,
        content_block: { type: 'text', text: '' },
      });
      for (const chunk of split(text)) {
        if (DELAY) await sleep(DELAY);
        sse(res, { type: 'content_block_delta', index, delta: { type: 'text_delta', text: chunk } });
      }
      sse(res, { type: 'content_block_stop', index });
    };

    // 思考块（thinking）：与真实 provider 一样单独一个 content block。
    const emitThinking = async (index, text) => {
      sse(res, {
        type: 'content_block_start', index,
        content_block: { type: 'thinking', thinking: '' },
      });
      for (const chunk of split(text)) {
        if (DELAY) await sleep(DELAY);
        sse(res, { type: 'content_block_delta', index, delta: { type: 'thinking_delta', thinking: chunk } });
      }
      sse(res, { type: 'content_block_stop', index });
    };

    if (!hasResult) {
      await emitThinking(0, 'The user asks for a short answer. I should read the README first, then reply.');
      await emitText(1, 'I will read the README first, then answer.');
      sse(res, {
        type: 'content_block_start', index: 2,
        content_block: { type: 'tool_use', id: 'toolu_mock1', name: 'read_file', input: {} },
      });
      const args = JSON.stringify({ path: 'Cargo.toml', limit: 5 });
      for (const chunk of split(args)) {
        sse(res, { type: 'content_block_delta', index: 2, delta: { type: 'input_json_delta', partial_json: chunk } });
      }
      sse(res, { type: 'content_block_stop', index: 2 });
    } else {
      await emitThinking(0, 'The README has been read. Now I can answer.');
      await emitText(1, 'Done. The workspace has a Cargo workspace with two crates.');
    }

    sse(res, {
      type: 'message_delta',
      delta: { stop_reason: hasResult ? 'end_turn' : 'tool_use', stop_sequence: null },
      usage: { output_tokens: 21 },
    });
    sse(res, { type: 'message_stop' });
    res.end();
  });
});

function split(s, n = 12) {
  const out = [];
  for (let i = 0; i < s.length; i += n) out.push(s.slice(i, i + n));
  return out;
}

server.listen(PORT, '127.0.0.1', () => {
  console.log(`mock anthropic llm on http://127.0.0.1:${PORT}/v1/messages`);
});
