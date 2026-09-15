// Golden frames for the OpenAI Chat Completions endpoint: the original's
// `createSseTranslator` fed a set of upstream event streams.
const { slice, evaluate, write } = require('./fixtures');

const region = slice('function createSseTranslator', '// ── HTTP 请求处理');
const api = evaluate(region, ['log'], { log: () => {} }, ['createSseTranslator']);
const { createSseTranslator } = api;

const MODEL = 'deepseek/deepseek-v4-flash';
const ID = 'chatcmpl-fixture';
const CREATED = 1700000000;

const cases = [
  {
    name: 'plain text with a terminal usage report',
    lines: [
      '{"type":"start"}',
      '{"type":"text-start"}',
      '{"type":"text-delta","text":"Hello"}',
      '{"type":"text-delta","text":", world"}',
      '{"type":"text-end"}',
      '{"type":"finish-step","finishReason":"stop","usage":{"inputTokens":10,"outputTokens":3,"cachedInputTokens":4}}',
      '{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":10,"outputTokens":3,"cachedInputTokens":4}}',
    ],
  },
  {
    name: 'reasoning before text',
    lines: [
      '{"type":"reasoning-start"}',
      '{"type":"reasoning-delta","text":"thinking"}',
      '{"type":"reasoning-end"}',
      '{"type":"text-delta","text":"answer"}',
      '{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":5,"outputTokens":2}}',
    ],
  },
  {
    name: 'a tool call carries index id and arguments',
    lines: [
      '{"type":"tool-input-start"}',
      '{"type":"tool-input-delta"}',
      '{"type":"tool-input-end"}',
      '{"type":"tool-call","toolCallId":"call_a","toolName":"shell_output","input":{"cmd":"ls"}}',
      '{"type":"finish","finishReason":"tool-calls","totalUsage":{"inputTokens":7,"outputTokens":9}}',
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
  {
    name: 'an upstream error yields no frames from the parser',
    lines: [
      '{"type":"text-delta","text":"partial"}',
      '{"type":"error","error":{"message":"<429> slow down","code":"RATE_LIMITED"}}',
    ],
  },
];

const out = {
  source: 'commandcode-proxy/proxy.mjs (createSseTranslator region, extracted verbatim)',
  note: 'frames are the parser output only; the original writes [DONE] from the handler',
  model: MODEL,
  id: ID,
  created: CREATED,
  cases: cases.map((entry) => {
    const translator = createSseTranslator(MODEL, ID, CREATED);
    const frames = [];
    for (const line of entry.lines) {
      const produced = translator.parseLine(line);
      if (produced) frames.push(...produced);
    }
    return {
      name: entry.name,
      lines: entry.lines,
      frames,
      done: translator.getDoneEvent(),
      output_tokens: translator.outputTokens,
      input_tokens: translator.inputTokens,
      cached_input_tokens: translator.cachedInputTokens,
    };
  }),
};

write('chat_sse_golden.json', out);
