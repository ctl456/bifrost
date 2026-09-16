// Golden fixtures for the OpenAI Responses endpoint: the original's
// `convertResponsesToChat` + `buildCcRequest` for the request side,
// `buildResponsesObject` for the complete body, and `createResponsesSseTranslator`
// for the stream.
const nodeCrypto = require('crypto');
const { slice, evaluate, write } = require('./fixtures');

// `completed_at` comes from the clock, so the clock is pinned for the run: a
// fixture that changed on every regeneration would be no evidence at all.
const FIXED_MS = 1_700_000_000_000;
Date.now = () => FIXED_MS;

const deviceProfile = slice('const DEVICE_PROFILE = {', 'const FP_OS_USERS');
// From the utilities down to `handleResponses`: the CC body builder, both OpenAI
// translators and the whole Responses section.
const conversions = slice('function slugifyProjectPath(p)', 'async function handleResponses');

const api = evaluate(
  `${deviceProfile}\n${conversions}`,
  ['CFG', 'log', 'crypto', 'randomUUID'],
  {
    CFG: { emptySystemPlaceholder: true, cliMode: 'agent', deviceProjectDir: '' },
    log: () => {},
    crypto: nodeCrypto,
    randomUUID: nodeCrypto.randomUUID,
  },
  [
    'buildCcRequest',
    'convertResponsesToChat',
    'buildResponsesObject',
    'createResponsesSseTranslator',
    'mapFinishReason',
  ]
);
const { buildCcRequest, convertResponsesToChat, buildResponsesObject, createResponsesSseTranslator, mapFinishReason } = api;

const MODEL = 'deepseek/deepseek-v4-flash';
const ID = 'resp_fixture';
const CREATED = 1_700_000_000;

// ── request conversion ───────────────────────────────────────────────────────
const requestCases = [
  {
    name: 'a plain string input is one user turn',
    request: { model: MODEL, input: 'hello' },
  },
  {
    name: 'an item with a role but no type is still a message',
    request: {
      input: [
        { role: 'user', content: 'first' },
        { type: 'message', role: 'user', content: [{ type: 'input_text', text: 'second' }] },
      ],
    },
  },
  {
    name: 'instructions become the system prompt',
    request: { instructions: 'be brief', input: 'hi' },
  },
  {
    name: 'instructions may be block shaped',
    request: {
      instructions: [{ type: 'input_text', text: 'stable ' }, { type: 'input_text', text: 'prefix' }],
      input: 'hi',
    },
  },
  {
    name: 'a developer message item becomes a system section too',
    request: {
      instructions: 'first section',
      input: [
        { type: 'message', role: 'developer', content: 'second section' },
        { type: 'message', role: 'user', content: 'hi' },
      ],
    },
  },
  {
    name: 'reasoning a message and its calls become one assistant turn',
    request: {
      input: [
        { type: 'message', role: 'user', content: 'search for cats' },
        { type: 'reasoning', summary: [{ type: 'summary_text', text: 'I should ' }, { type: 'summary_text', text: 'search' }] },
        { type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'on it' }] },
        { type: 'function_call', call_id: 'call_1', name: 'search', arguments: '{"q":"cats"}' },
        { type: 'function_call', call_id: 'call_2', name: 'fetch', arguments: { url: 'https://x' } },
        { type: 'function_call_output', call_id: 'call_1', output: 'found 3' },
        { type: 'function_call_output', call_id: 'call_2', output: { count: 2 } },
      ],
    },
  },
  {
    name: 'a reasoning item without an answer is not a turn',
    request: {
      input: [
        { type: 'reasoning', text: 'thinking with nothing to say' },
        { type: 'message', role: 'user', content: 'hi' },
      ],
    },
  },
  {
    name: 'an unparseable argument string becomes an empty object',
    request: {
      input: [{ type: 'function_call', call_id: 'call_1', name: 'read', arguments: 'not json' }],
    },
  },
  {
    name: 'a message the client addressed with no content is still a turn',
    request: { input: [{ type: 'message', role: 'user', content: [] }] },
  },
  {
    name: 'function tools are forwarded with their schemas',
    request: {
      input: 'hi',
      tools: [
        { type: 'function', name: 'bash_output', description: 'read output', parameters: { type: 'object', properties: {} } },
        { type: 'function', name: 'read_multiple_files' },
        { name: 'named-without-a-type' },
        { type: 'web_search' },
      ],
    },
  },
  {
    name: 'a tool choice mode is carried',
    request: { input: 'hi', tool_choice: 'required' },
  },
  {
    name: 'a forced tool is named',
    request: { input: 'hi', tool_choice: { type: 'function', name: 'search' } },
  },
  {
    name: 'every sampling parameter is forwarded',
    request: {
      model: MODEL,
      input: 'hi',
      max_output_tokens: 1234,
      temperature: 0.25,
      top_p: 0.9,
      parallel_tool_calls: false,
      reasoning: { effort: 'high' },
    },
  },
  {
    name: 'a reasoning configuration without an effort sends no level',
    request: { input: 'hi', reasoning: { summary: 'auto' } },
  },
  {
    name: 'an unknown item type is dropped without failing the request',
    request: {
      input: [
        { type: 'brand_new_item', content: 'x' },
        { type: 'message', role: 'user', content: 'kept' },
      ],
    },
  },
];

const params = {
  source: 'commandcode-proxy/proxy.mjs (convertResponsesToChat + buildCcRequest, extracted verbatim)',
  note: 'compare against the full Bifrost path: Responses decode then cc/1.53.1 encode',
  cases: requestCases.map((entry) => ({
    name: entry.name,
    request: entry.request,
    params: buildCcRequest(convertResponsesToChat(entry.request)).params,
  })),
};

// ── complete response bodies ─────────────────────────────────────────────────
// The handler assembles its inputs inline, so the assembly is mirrored here; the
// bodies themselves come from the original's own `buildResponsesObject`.
function echoOptions(respReq) {
  return {
    instructions: respReq.instructions === undefined ? null : respReq.instructions,
    max_output_tokens: respReq.max_output_tokens === undefined ? null : respReq.max_output_tokens,
    temperature: respReq.temperature,
    top_p: respReq.top_p,
    reasoning: respReq.reasoning || null,
    tool_choice: typeof respReq.tool_choice === 'string' ? respReq.tool_choice : 'auto',
    tools: respReq.tools || [],
  };
}

function collect(lines) {
  let fullText = '';
  let thinkingText = '';
  const toolCalls = [];
  let usage = null;
  let finishReason = 'stop';
  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed || trimmed === '[DONE]' || trimmed.startsWith(':')) continue;
    const event = JSON.parse(trimmed);
    switch (event.type) {
      case 'text-delta': fullText += event.text || ''; break;
      case 'reasoning-delta': thinkingText += event.text || ''; break;
      case 'tool-call':
        toolCalls.push({
          id: event.toolCallId || 'call_missing',
          type: 'function',
          function: {
            name: event.toolName || '',
            arguments: typeof event.input === 'string' ? event.input : JSON.stringify(event.input || {}),
          },
        });
        break;
      case 'finish':
        finishReason = mapFinishReason(event.finishReason || 'stop');
        if (event.totalUsage || event.usage) usage = event.totalUsage || event.usage;
        break;
      default: break;
    }
  }
  return { fullText, thinkingText, toolCalls, usage, finishReason };
}

const bodyCases = [
  {
    name: 'a plain answer with its accounting',
    request: { model: MODEL, input: 'hi' },
    lines: [
      '{"type":"text-delta","text":"Hello"}',
      '{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":10,"outputTokens":3,"cachedInputTokens":4}}',
    ],
  },
  {
    name: 'reasoning is reported as its own item',
    request: { instructions: 'be brief', input: 'hi', reasoning: { effort: 'high', summary: 'auto' } },
    lines: [
      '{"type":"reasoning-delta","text":"thinking"}',
      '{"type":"text-delta","text":"answer"}',
      '{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":5,"outputTokens":2}}',
    ],
  },
  {
    name: 'calls are reported as their own items with their arguments',
    request: { input: 'hi', tools: [{ type: 'function', name: 'shell_output' }] },
    lines: [
      '{"type":"text-delta","text":"looking"}',
      '{"type":"tool-call","toolCallId":"call_a","toolName":"shell_output","input":{"cmd":"ls"}}',
      '{"type":"finish","finishReason":"tool-calls","totalUsage":{"inputTokens":7,"outputTokens":9}}',
    ],
  },
  {
    name: 'a turn cut off by the token limit is incomplete',
    request: { input: 'hi', max_output_tokens: 16 },
    lines: [
      '{"type":"text-delta","text":"half an ans"}',
      '{"type":"finish","finishReason":"length","totalUsage":{"inputTokens":3,"outputTokens":4}}',
    ],
  },
  {
    name: 'a call the client answered keeps the name the call carried',
    request: { input: 'hi' },
    lines: [
      '{"type":"tool-call","toolCallId":"call_z","toolName":"read","input":"{}"}',
      '{"type":"finish","finishReason":"tool-calls","totalUsage":{"inputTokens":2,"outputTokens":3}}',
    ],
  },
  {
    name: 'a zero-output turn reports no input either',
    request: { input: 'hi' },
    lines: ['{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":9,"cachedInputTokens":9,"outputTokens":0}}'],
  },
];

const bodyEntries = bodyCases.map((entry) => {
    const collected = collect(entry.lines);
    return {
      name: entry.name,
      request: entry.request,
      lines: entry.lines,
      response: positionalBody(
        buildResponsesObject(
          ID,
          MODEL,
          CREATED,
          collected.fullText,
          collected.thinkingText,
          collected.toolCalls,
          collected.usage,
          Object.assign(echoOptions(entry.request), { finishReason: collected.finishReason })
        )
      ),
    };
  });

// `completed_at` is the pinned clock's reading, not an input the handler is
// given: restating it here would let the fixture claim a value the bodies never
// carry, so it is read back from what the bodies actually say.
const completedAt = bodyEntries[0].response.completed_at;
for (const entry of bodyEntries) {
  if (entry.response.completed_at !== completedAt) {
    throw new Error(`the pinned clock yielded more than one completed_at: ${entry.name} says ${entry.response.completed_at}, the first case says ${completedAt}`);
  }
}

const bodies = {
  source: 'commandcode-proxy/proxy.mjs (convertResponsesToChat + buildResponsesObject, extracted verbatim)',
  note: "the handler assembles the body inputs inline, which the generator mirrors; the body is the original's, with item ids rewritten to the positional form Bifrost mints",
  model: MODEL,
  id: ID,
  created: CREATED,
  completed: completedAt,
  cases: bodyEntries,
};

// ── streams ─────────────────────────────────────────────────────────────────
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

/**
 * Rewrite the item ids the original minted to the positional form Bifrost uses.
 *
 * The original mints a UUID per item, so its frames are not reproducible and no
 * two runs of this generator would agree. An id only has to be unique within the
 * response it names, and Bifrost names items by the index they are reported at,
 * so the ids are mapped to that — first item to open is 0, and the same item
 * keeps its number wherever it appears. Every other byte of every frame is
 * verbatim, including the `sequence_number` the original counted.
 */
function positionalItem(item, index) {
  if (typeof item.id === 'string' && /^(msg|rs|fc)_/.test(item.id)) {
    return Object.assign({}, item, { id: item.id.replace(/^(msg|rs|fc)_.*/, `$1_${index}`) });
  }
  return item;
}

/** The same rewriting for a complete body, whose item ids are UUIDs too. */
function positionalBody(response) {
  return Object.assign({}, response, { output: (response.output || []).map(positionalItem) });
}

function positionalIds(frames) {
  const assigned = new Map();
  return frames.map((frame) =>
    frame.replace(/"(msg|rs|fc)_[0-9a-f-]{24,36}"/g, (whole, prefix) => {
      if (!assigned.has(whole)) assigned.set(whole, `"${prefix}_${assigned.size}"`);
      return assigned.get(whole);
    })
  );
}

function consume(lines) {
  const translator = createResponsesSseTranslator(MODEL, ID, CREATED);
  const frames = [];
  for (const line of lines) {
    const produced = translator.parseLine(line);
    if (produced) frames.push(...produced);
  }
  return { frames, translator };
}

function translate(lines) {
  const consumed = consume(lines);
  consumed.frames.push(...consumed.translator.finish());
  return { frames: positionalIds(consumed.frames), translator: consumed.translator };
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
      '{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":10,"outputTokens":3,"cachedInputTokens":4}}',
    ],
  },
  {
    name: 'reasoning opens its own item before the answer',
    lines: [
      '{"type":"reasoning-delta","text":"think"}',
      '{"type":"reasoning-delta","text":"ing"}',
      '{"type":"text-delta","text":"answer"}',
      '{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":5,"outputTokens":2}}',
    ],
  },
  {
    name: 'a tool call opens an item whose opening frame has no arguments yet',
    lines: [
      '{"type":"text-delta","text":"looking"}',
      '{"type":"tool-call","toolCallId":"call_a","toolName":"search","input":{"q":"cats"}}',
      '{"type":"finish","finishReason":"tool-calls","totalUsage":{"inputTokens":7,"outputTokens":9}}',
    ],
  },
  // A case where the terminal event omits its reason is *not* recorded here: the
  // original's Responses translator has no `finish-step` arm, so it reports a
  // turn the step called `length` as `completed`, while Bifrost keeps the step
  // reason and reports `incomplete`. That is a divergence on purpose, so it is
  // covered by `tests/responses_sse.rs` instead of being frozen into a fixture
  // this file would have to reproduce.
  {
    name: 'a truncated turn ends on response.incomplete',
    lines: [
      '{"type":"text-delta","text":"half"}',
      '{"type":"finish","finishReason":"length","totalUsage":{"inputTokens":3,"outputTokens":1}}',
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
    name: 'an empty delta opens nothing',
    lines: ['{"type":"text-delta","text":""}', '{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":1,"outputTokens":1}}'],
  },
];

const ERROR_LINES = [
  '{"type":"text-delta","text":"partial"}',
  '{"type":"error","error":{"message":"<429> slow down","code":"RATE_LIMITED"}}',
];
const SILENT_LINES = ['{"type":"start"}', '{"type":"finish","finishReason":"stop"}'];

const cases = streamCases.map((entry) => {
  const { frames } = translate(entry.lines);
  return { name: entry.name, lines: entry.lines, frames };
});

// A stream that failed: the translator frames the failure, which Bifrost hands to
// the edge instead — so only the payload shape is compared.
/**
 * The failure frames are read *before* `finish()`, from the state a failing
 * stream is actually in.
 *
 * The original only sends a terminal event inside `finish()`, and the edge never
 * reaches it when the upstream breaks: it frames the failure instead. Recording
 * these after a `finish()` would snapshot a counter the failing path never has,
 * and one that Bifrost cannot reproduce either — Bifrost suppresses the terminal
 * event outright when the upstream errored, so its counter stops where this one
 * does. Each frame is taken from its own translator, because both `fail()` and
 * `errorEvent()` advance the counter.
 */
const failure = consume(ERROR_LINES);
const quiet = translate(SILENT_LINES);
const failedFrames = positionalIds(consume(ERROR_LINES).translator.fail('slow down'));
const errorFrame = consume(ERROR_LINES).translator.errorEvent('slow down');

write('responses_params_golden.json', params);
write('responses_body_golden.json', bodies);
write('responses_sse_golden.json', {
  source: 'commandcode-proxy/proxy.mjs (createResponsesSseTranslator, extracted verbatim)',
  note: 'item ids are rewritten to the positional form Bifrost mints; every other byte is verbatim. `error_case.frames` is the stream as it stood when the upstream broke, before the terminal event the original would have appended and Bifrost does not send.',
  model: MODEL,
  id: ID,
  created: CREATED,
  cases,
  error_case: { lines: ERROR_LINES, frames: positionalIds(failure.frames), failed_frames: failedFrames, error_frame: errorFrame },
  silent_case: { lines: SILENT_LINES, frames: quiet.frames },
});
