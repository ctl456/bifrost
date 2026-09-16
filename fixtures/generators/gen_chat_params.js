// Golden `params` for the OpenAI Chat Completions endpoint: the original's
// `buildCcRequest` run over a set of client request bodies, so the Rust path
// (decode → encode) can be diffed against it.
const { slice, evaluate, write } = require('./fixtures');

const deviceProfile = slice('const DEVICE_PROFILE = {', 'const FP_OS_USERS');
const conversions = slice('function slugifyProjectPath(p)', 'function createSseTranslator');

const api = evaluate(
  `${deviceProfile}\n${conversions}`,
  ['CFG'],
  { CFG: { emptySystemPlaceholder: true, cliMode: 'agent', deviceProjectDir: '' } },
  ['buildCcRequest']
);
const { buildCcRequest } = api;

const cases = [
  {
    name: 'plain user turn with no system prompt',
    request: { model: 'deepseek/deepseek-v4-flash', messages: [{ role: 'user', content: 'hello' }], stream: true },
  },
  {
    name: 'system and developer turns are joined as sections',
    request: {
      messages: [
        { role: 'system', content: 'first section' },
        { role: 'developer', content: [{ type: 'text', text: 'second section' }] },
        { role: 'system', content: 'third section' },
        { role: 'user', content: 'hi' },
      ],
    },
  },
  {
    name: 'an empty system section with a breakpoint is kept',
    request: {
      messages: [
        { role: 'system', content: [{ type: 'text', text: '', cache_control: { type: 'ephemeral' } }] },
        { role: 'system', content: 'tail' },
        { role: 'user', content: 'hi' },
      ],
    },
  },
  {
    name: 'a multimodal user turn keeps part order',
    request: {
      messages: [
        {
          role: 'user',
          content: [
            { type: 'text', text: 'look at these' },
            { type: 'image_url', image_url: { url: 'data:image/png;base64,AAAA' } },
            { type: 'image_url', image_url: { url: 'https://example.test/a.jpg', detail: 'high' } },
          ],
        },
      ],
    },
  },
  {
    name: 'an assistant turn keeps reasoning text then calls',
    request: {
      messages: [
        { role: 'user', content: 'search' },
        {
          role: 'assistant',
          reasoning_content: 'I should search',
          content: 'on it',
          tool_calls: [
            { id: 'call_1', type: 'function', function: { name: 'search', arguments: '{"q":"cats"}' } },
            { id: 'call_2', type: 'function', function: { name: 'fetch', arguments: { url: 'https://x' } } },
          ],
        },
        { role: 'tool', tool_call_id: 'call_1', content: 'found 3' },
        {
          role: 'tool',
          tool_call_id: 'call_2',
          name: 'fallback_name',
          content: [
            { type: 'text', text: 'a' },
            { type: 'text', text: 'b' },
          ],
        },
        { role: 'tool', tool_call_id: 'call_missing', content: 'orphan' },
      ],
    },
  },
  {
    name: 'assistant content parts keep their own order',
    request: {
      messages: [
        {
          role: 'assistant',
          content: [
            { type: 'text', text: 'part one' },
            { type: 'reasoning', text: 'inline thinking' },
            { type: 'text', text: 'part two', cache_control: { type: 'ephemeral' } },
          ],
        },
      ],
    },
  },
  {
    name: 'tool definitions are aliased and defaulted',
    request: {
      messages: [{ role: 'user', content: 'hi' }],
      tools: [
        { type: 'function', function: { name: 'bash_output', description: 'read output' } },
        { type: 'function', function: { name: 'read_multiple_files', description: '', parameters: { type: 'object' } } },
        { type: 'function', function: { name: 'plain' } },
      ],
    },
  },
  {
    name: 'every sampling parameter is forwarded',
    request: {
      messages: [{ role: 'user', content: 'hi' }],
      max_tokens: 1234,
      temperature: 0.25,
      reasoning_effort: 'high',
      parallel_tool_calls: false,
      tool_choice: 'required',
      prompt_cache_key: 'cache-me',
    },
  },
  {
    name: 'a prompt cache key marks the last section when the client marked none',
    request: {
      messages: [
        { role: 'system', content: 'stable prefix' },
        { role: 'user', content: 'hi' },
      ],
      prompt_cache_key: 'bucket',
    },
  },
  {
    name: 'an unknown role and unknown parts degrade instead of failing',
    request: {
      messages: [
        { role: 'observer', content: 'wat' },
        { role: 'user', content: [{ type: 'video', url: 'x' }] },
        { role: 'assistant', content: [{ type: 'audio' }, { type: 'text', text: 'kept' }] },
      ],
    },
  },
  {
    name: 'tool choice spellings normalise onto the wire vocabulary',
    request: {
      messages: [{ role: 'user', content: 'hi' }],
      tools: [{ type: 'function', function: { name: 'search' } }],
      tool_choice: { type: 'function', function: { name: 'search' } },
    },
  },
  {
    name: 'max tokens above the cap is clamped',
    request: { messages: [{ role: 'user', content: 'hi' }], max_tokens: 999999 },
  },
  {
    name: 'a client breakpoint wins over the prompt cache key',
    request: {
      messages: [
        { role: 'system', content: [{ type: 'text', text: 'marked', cache_control: { type: 'ephemeral' } }] },
        { role: 'system', content: 'unmarked' },
        { role: 'user', content: 'hi' },
      ],
      prompt_cache_key: 'cache-bucket-1',
    },
  },
  // No `max_completion_tokens` case: the original destructures `max_tokens` only
  // and silently ignores the newer spelling, which Bifrost keeps. A departure
  // cannot live in a fixture generated from the original.
];

const out = {
  source: 'commandcode-proxy/proxy.mjs (buildCcRequest and its helpers, extracted verbatim)',
  note: 'compare against the full Bifrost path: protocol decode then cc/1.53.1 encode',
  cases: cases.map((entry) => ({
    name: entry.name,
    request: entry.request,
    params: buildCcRequest(entry.request).params,
  })),
};

write('chat_params_golden.json', out);
