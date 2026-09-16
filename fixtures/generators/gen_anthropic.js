// Golden fixtures for the Anthropic Messages endpoint: the original's
// `convertAnthropicToOpenAI` + `buildCcRequest` for the request side, and its
// `createAnthropicSseTranslator` for the stream.
const nodeCrypto = require('crypto');
const { slice, evaluate, write } = require('./fixtures');

const deviceProfile = slice('const DEVICE_PROFILE = {', 'const FP_OS_USERS');
// From the utilities down to the model list: the CC body builder, the OpenAI
// translator and the whole Anthropic section.
const conversions = slice('function slugifyProjectPath(p)', '// ── 动态模型列表');

const api = evaluate(`${deviceProfile}\n${conversions}`, ['CFG', 'log', 'crypto', 'randomUUID', 'createIdleWatchdog', 'STREAM_IDLE_TIMEOUT_MS'], {
  CFG: { emptySystemPlaceholder: true, cliMode: 'agent', deviceProjectDir: '' },
  log: () => {},
  // The section reaches for the Node module, not the web-crypto global.
  crypto: nodeCrypto,
  randomUUID: nodeCrypto.randomUUID,
  // The generator drives the translator directly, so the watchdog never fires.
  createIdleWatchdog: () => ({ arm: () => new Promise(() => {}), dispose() {} }),
  STREAM_IDLE_TIMEOUT_MS: 30000,
}, ['convertAnthropicToOpenAI', 'buildCcRequest', 'createAnthropicSseTranslator']);
const { buildCcRequest, convertAnthropicToOpenAI, createAnthropicSseTranslator } = api;

const MODEL = 'claude-sonnet-4-6';
const ID = 'msg_fixture';

const requestCases = [
  {
    name: 'a plain user turn with no system prompt',
    request: { model: 'claude-sonnet-4-6', max_tokens: 1024, messages: [{ role: 'user', content: 'hello' }] },
  },
  {
    name: 'a string prompt is one section',
    request: { system: 'be brief', messages: [{ role: 'user', content: 'hi' }] },
  },
  {
    name: 'prompt sections keep their breakpoints',
    request: {
      system: [
        { type: 'text', text: 'stable prefix', cache_control: { type: 'ephemeral' } },
        { type: 'text', text: 'volatile tail' },
      ],
      messages: [{ role: 'user', content: 'hi' }],
    },
  },
  {
    name: 'an empty prompt section is no prompt at all',
    request: {
      system: [{ type: 'text', text: '', cache_control: { type: 'ephemeral' } }],
      messages: [{ role: 'user', content: 'hi' }],
    },
  },
  {
    name: 'a multimodal user turn keeps its part order',
    request: {
      messages: [
        {
          role: 'user',
          content: [
            { type: 'text', text: 'look at these' },
            { type: 'image', source: { type: 'base64', media_type: 'image/jpeg', data: 'AAAA' } },
            { type: 'image', source: { type: 'url', url: 'https://example.test/a.png' } },
          ],
        },
      ],
    },
  },
  {
    name: 'a thinking turn keeps reasoning text and calls in order',
    request: {
      messages: [
        { role: 'user', content: 'search for cats' },
        {
          role: 'assistant',
          content: [
            { type: 'thinking', thinking: 'I should ' },
            { type: 'thinking', thinking: 'search' },
            { type: 'text', text: 'on it' },
            { type: 'tool_use', id: 'toolu_1', name: 'search', input: { q: 'cats' } },
            { type: 'tool_use', id: 'toolu_2', name: 'fetch', input: {} },
          ],
        },
        {
          role: 'user',
          content: [
            { type: 'text', text: 'and now?' },
            { type: 'tool_result', tool_use_id: 'toolu_1', content: 'found 3' },
          ],
        },
      ],
    },
  },
  {
    name: 'a tool result may carry text blocks',
    request: {
      messages: [
        { role: 'assistant', content: [{ type: 'tool_use', id: 'toolu_1', name: 'read', input: {} }] },
        {
          role: 'user',
          content: [
            {
              type: 'tool_result',
              tool_use_id: 'toolu_1',
              content: [{ type: 'text', text: 'first' }, { type: 'text', text: 'second' }, { type: 'image' }],
              is_error: true,
            },
          ],
        },
      ],
    },
  },
  {
    name: 'a tool result for a call the client trimmed away carries no name',
    request: {
      messages: [{ role: 'user', content: [{ type: 'tool_result', tool_use_id: 'toolu_gone', content: 'orphan' }] }],
    },
  },
  {
    name: 'an assistant turn of two parts keeps both as blocks',
    request: {
      messages: [
        {
          role: 'assistant',
          content: [
            { type: 'text', text: 'one' },
            { type: 'text', text: 'two', cache_control: { type: 'ephemeral' } },
          ],
        },
      ],
    },
  },
  {
    name: 'a single empty assistant part is not content',
    request: { messages: [{ role: 'assistant', content: [{ type: 'text', text: '' }] }] },
  },
  {
    name: 'tool definitions are forwarded with their schemas',
    request: {
      messages: [{ role: 'user', content: 'hi' }],
      tools: [
        { name: 'bash_output', description: 'read output' },
        { name: 'read_multiple_files', description: '', input_schema: { type: 'object', properties: {} } },
      ],
    },
  },
  {
    name: 'every tool choice spelling is understood',
    request: { messages: [{ role: 'user', content: 'hi' }], tool_choice: { type: 'any' } },
  },
  {
    name: 'a forced tool is named',
    request: { messages: [{ role: 'user', content: 'hi' }], tool_choice: { type: 'tool', name: 'search' } },
  },
  {
    name: 'every sampling parameter is carried',
    request: {
      model: 'claude-opus-4-1',
      max_tokens: 8192,
      temperature: 0.25,
      top_p: 0.9,
      stop_sequences: ['\n\n'],
      stream: true,
      metadata: { user_id: 'user_1' },
      messages: [{ role: 'user', content: 'hi' }],
    },
  },
  {
    name: 'a thinking budget becomes an effort level',
    request: {
      messages: [{ role: 'user', content: 'hi' }],
      thinking: { type: 'enabled', budget_tokens: 12000 },
    },
  },
  {
    name: 'an adaptive thinking budget carries its own level',
    request: {
      messages: [{ role: 'user', content: 'hi' }],
      thinking: { type: 'adaptive', effort: 'max' },
    },
  },
  {
    name: 'a disabled thinking budget sends no level at all',
    request: { messages: [{ role: 'user', content: 'hi' }], thinking: { type: 'disabled' } },
  },
  {
    name: 'unexpected shapes are dropped rather than rejected',
    request: {
      system: 12,
      messages: [
        { role: 'system', content: 'ignored' },
        { role: 'user', content: [{ type: 'video', url: 'x' }] },
        { role: 'assistant', content: [{ type: 'audio' }] },
      ],
      tool_choice: { type: 'sometimes' },
    },
  },
];

const params = {
  source: 'commandcode-proxy/proxy.mjs (convertAnthropicToOpenAI + buildCcRequest, extracted verbatim)',
  cases: requestCases.map((entry) => ({
    name: entry.name,
    request: entry.request,
    params: buildCcRequest(convertAnthropicToOpenAI(entry.request)).params,
  })),
};

/** A response whose body yields the given lines, one read per line. */
function makeResponse(lines) {
  const encoder = new TextEncoder();
  let index = 0;
  return {
    body: {
      getReader() {
        return {
          read: async () =>
            index < lines.length ? { done: false, value: encoder.encode(`${lines[index++]}\n`) } : { done: true },
          cancel: async () => {},
        };
      },
    },
  };
}

async function translate(lines) {
  const ctx = { bytesReceived: 0, lastCcEvent: '', inputTokens: 0, outputTokens: 0, cachedInputTokens: 0, upstreamError: null };
  const generator = createAnthropicSseTranslator(makeResponse(lines), MODEL, ID, ctx);
  const frames = [];
  for await (const frame of generator) frames.push(frame);
  return { frames, ctx };
}

const streamCases = [
  {
    name: 'plain text with a terminal usage report',
    lines: [
      '{"type":"start"}',
      '{"type":"text-start"}',
      '{"type":"text-delta","text":"Hello"}',
      '{"type":"text-delta","text":", world"}',
      '{"type":"text-end"}',
      '{"type":"finish-step","finishReason":"stop","usage":{"inputTokens":10,"outputTokens":3,"cachedInputTokens":4,"inputTokenDetails":{"noCacheTokens":6}}}',
      '{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":10,"outputTokens":3,"cachedInputTokens":4,"inputTokenDetails":{"noCacheTokens":6}}}',
    ],
  },
  {
    name: 'reasoning becomes a signed thinking block',
    lines: [
      '{"type":"reasoning-start"}',
      '{"type":"reasoning-delta","text":"think"}',
      '{"type":"reasoning-end"}',
      '{"type":"text-delta","text":"answer"}',
      '{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":5,"outputTokens":2}}',
    ],
  },
  {
    name: 'a thinking-only turn reports the failure it is',
    lines: [
      '{"type":"reasoning-delta","text":"only thinking"}',
      '{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":5,"outputTokens":0}}',
    ],
  },
  {
    name: 'a tool call is its own block',
    lines: [
      '{"type":"text-delta","text":"looking"}',
      '{"type":"tool-call","toolCallId":"toolu_a","toolName":"search","input":{"q":"cats"}}',
      '{"type":"finish","finishReason":"tool-calls","totalUsage":{"inputTokens":7,"outputTokens":9,"cachedInputTokens":2,"inputTokenDetails":{"noCacheTokens":5,"cacheWriteTokens":3}}}',
    ],
  },
  {
    name: 'the step reason is used when the terminal event omits one',
    lines: [
      '{"type":"text-delta","text":"hi"}',
      '{"type":"finish-step","finishReason":"length","usage":{"inputTokens":3,"outputTokens":1}}',
      '{"type":"finish","totalUsage":{"inputTokens":3,"outputTokens":1}}',
    ],
  },
  {
    name: 'silent and unknown events produce no frames',
    lines: [
      '{"type":"provider-metadata","provider":"x"}',
      '{"type":"brand-new-event"}',
      ': keepalive',
      '[DONE]',
      '{"type":"text-delta","text":"after noise"}',
      '{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":1,"outputTokens":1}}',
    ],
  },
];

const ERROR_LINES = [
  '{"type":"text-delta","text":"partial"}',
  '{"type":"error","error":{"message":"<429> slow down","code":"RATE_LIMITED"}}',
];
const SILENT_LINES = ['{"type":"start"}', '{"type":"finish","finishReason":"stop"}'];

(async () => {
  const cases = [];
  for (const entry of streamCases) {
    const { frames, ctx } = await translate(entry.lines);
    cases.push({
      name: entry.name,
      lines: entry.lines,
      frames,
      output_tokens: ctx.outputTokens,
      input_tokens: ctx.inputTokens,
    });
  }

  // The failure cases: the translator frames the upstream's error, which Bifrost
  // hands to the edge instead — so only the payload shape is compared.
  const error = await translate(ERROR_LINES);
  const silent = await translate(SILENT_LINES);

  write('anthropic_params_golden.json', params);
  write('anthropic_sse_golden.json', {
    source: 'commandcode-proxy/proxy.mjs (createAnthropicSseTranslator, extracted verbatim)',
    note: 'frames are the translator output; the handler owns the HTTP status and the buffering of message_start',
    model: MODEL,
    id: ID,
    cases,
    error_case: {
      lines: ERROR_LINES,
      frames: error.frames,
      error_frame: error.frames[error.frames.length - 1],
    },
    silent_case: { lines: SILENT_LINES, frames: silent.frames },
  });
})().catch((error) => {
  console.error(error);
  process.exit(1);
});
