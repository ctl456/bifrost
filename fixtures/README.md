# Golden fixtures

Every `*.json` here was produced by running the original JavaScript from
`commandcode-proxy/proxy.mjs` verbatim, not by re-deriving its behavior from a
description. Executing the original is the point: a hand-written expectation
would encode the same misunderstanding as a hand-written port.

| Fixture | Extracted from | Checked by |
| --- | --- | --- |
| `chat_params_golden.json` | `buildCcRequest` and its helpers | `tests/chat_params.rs` |
| `chat_sse_golden.json` | `createSseTranslator` | `tests/chat_sse.rs` |
| `anthropic_params_golden.json` | `convertAnthropicToOpenAI` + `buildCcRequest` | `tests/anthropic_params.rs` |
| `anthropic_sse_golden.json` | `createAnthropicSseTranslator` | `tests/anthropic_sse.rs` |
| `responses_params_golden.json` | `convertResponsesToChat` + `buildCcRequest` | `tests/responses_params.rs` |
| `responses_body_golden.json` | `buildResponsesObject` | `tests/responses_body.rs` |
| `responses_sse_golden.json` | `createResponsesSseTranslator` | `tests/responses_sse.rs` |
| `fingerprint_golden.json` | the fingerprint region | `tests/fingerprint_golden.rs` |

The `params` fixtures pin the request side, which the Rust path reaches through
two layers (protocol decode, then the wire dialect encode); the `sse` fixtures pin
the response side, which it reaches through two more (wire decode, then protocol
render). One diverging byte in any of the four layers fails a test. The
fingerprint fixture holds the other direction: it pins what the module derives for
a set of API keys and salts, so a change there is caught by an expectation the
original produced rather than one this repository wrote down.

## Regenerating

The generators in `generators/` are not part of the build — cargo does not look
at this directory, because it holds no Rust targets — so run one by hand.

```sh
cd fixtures/generators
node gen_chat_params.js
node gen_chat_sse.js
node gen_anthropic.js
node gen_responses.js
```

The fingerprint fixture has no generator in this tree: it was recorded by slicing
the fingerprint region out of the same proxy by hand, and is regenerated the same
way when the reference implementation moves.

`CC_PROXY_SOURCE` overrides which proxy is read and defaults to
`../../../commandcode-proxy/proxy.mjs` relative to `generators/`, the checkout
this repository was built against.

Each generator slices the functions it needs out of the proxy and evaluates them
with the globals they close over (`CFG`, `log`, `crypto`, ...), which `fixtures.js`
does for all of them. That is why a fixture records what the original actually
does rather than what it is believed to do: the code under test is the original's.

## Normalized fields

Two fields are not reproducible from one run of the original to the next, so the
responses generators rewrite them and the fixture says so in its own header:

- **Item ids.** The original mints a UUID per output item, so two runs of the same
  stream differ. This proxy names an item by the index it is reported at, and the
  fixtures are rewritten to that form.
- **The clock.** `completed_at` is read from `Date.now()`, so `gen_responses.js`
  pins the clock for the run. `responses_body_golden.json` does not restate the
  value it expects: it reads `completed_at` back out of the bodies it produced, so
  a fixture cannot claim a time the bodies never carry.

`responses_sse_golden.json` also records its failure case from *before* the
original's `finish()`: a stream whose upstream broke never reaches it, so the
sequence counter is read in the state the failure path actually leaves it in.

## Changing a fixture

Treat a change to an existing expectation as a breaking change. These files are
the contract the Rust path is diffed against, and a case edited to match new
behavior is no longer evidence of anything; add a case instead, and regenerate so
the expectation comes from the original.

A behavior the port deliberately gets differently cannot live here at all — a
fixture generated from the original would contradict it by construction. Those
departures are listed in the header of the test that would otherwise carry the
case and asserted by a test of their own. See `tests/chat_params.rs` for
`max_completion_tokens`, `tests/anthropic_params.rs` for the synthesized tool call
id, and `tests/responses_sse.rs` for a truncation the terminal event omits.
