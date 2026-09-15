# Dialect alignment

The `cc` dialect in `bifrost-wire` is a reading of a published client. The client
moves on its own schedule, so the reading has to be re-checkable. This is what
`wire.drift_watch` points at when it logs a drift line, and what
`tools/check-dialect-alignment.sh` mechanizes.

## Comparing literals, not bytes

The published artifact is a ~2.5 MB minified bundle. A byte comparison against the
version the dialect was read from is useless: between `1.53.1` and `1.54.0` it
reports 304 added and 260 removed string literals, and every one of them is either
an esbuild `__name(fn,"sourceName")` stack-trace annotation, a renamed identifier
(`pR` → `lR`, `eA` → `YI`), or text from an unrelated feature. None of it is the
contract.

What carries the contract is the vocabulary: endpoint paths, header names, the
keys of the outgoing body, the lifecycle event name, the tool aliases. Those are
compared. Two forms, because the client writes the two differently:

- **quoted** — `"/alpha/generate"`, `"x-cmd-zdr"`, `"cli_session_exists"`. These
  sit in a table or a call, so their use count is stable, and a changed count means
  the shape moved rather than that the client grew.
- **bare** — `{workingDir:s,...,recentCommits:[]}`. Object keys are unquoted after
  minification, and short common ones (`sessionId`, `params`) collapse with
  unrelated code, so only presence is checked.

The `__name` annotations are stripped before either comparison, because they are
regenerated on every build and would otherwise drown the signal.

## The 1.54.0 check

`tools/check-dialect-alignment.sh 1.54.0` reports that nothing this dialect
implements has changed. The dialect stays at `cc/1.53.1`; a `cc/1.54.0` adapter
would be the same code under a new name.

Independently of the script, reading the release confirms it: the changelog for
1.54.0 has two entries, both `feat: command code has first-class herdr support
(#4704)`. `herdr` is a terminal multiplexer the client reports pane state to over a
unix socket; it does not touch `/alpha`. The new vocabulary the release adds
(`claimHerdrPane`, `buildHerdrLine`, `metadataParams`, `clampBytes`, `reportSession`)
belongs to that feature and to a compressor rename.

The strongest evidence is the outgoing generate body, which is byte-identical
between the two releases:

```js
body:{config:d,memory:null,taste:null,skills:null,permissionMode:u,
      threadId:toWireThreadId(t.threadId),mode:t.mode,promptCache:t.promptCache,
      params:{model:t.model,messages:l,tools:c,system:...,max_tokens:...,
              stream:!0,...temperature?,...reasoning_effort?}}
```

## Running it

```sh
tools/check-dialect-alignment.sh              # against the latest published tag
tools/check-dialect-alignment.sh 1.54.0       # against a named version
CC_SELFTEST=1 tools/check-dialect-alignment.sh 1.54.0
```

It fetches both the named version and the one the dialect was read from, strips
the debug annotations, and compares. It exits non-zero when something the dialect
implements is gone, when a quoted literal's use count moved, or when the literals
around an anchor window no longer match in order.

`CC_SELFTEST=1` injects a literal that cannot exist, so that run must fail. It is
there because a check that only ever passes is not evidence: the self-test is how
the check proves it is capable of disagreeing.

When it does report a change, the next step is to re-read the package and decide
whether the shape actually moved. If it did, add a new dialect — `cc/<version>`
in `bifrost-wire` — rather than editing `v1531.rs`, so the old one stays available
per key while the change rolls out.

## What the check cannot see

Three keys this proxy implements are absent from the client, so the published
package cannot confirm them. They are listed in the script's `OURS` table rather
than silently dropped:

- `/provider/v1/models` — read from `commandcode-proxy`; the client never calls it.
- `tool_choice`, `parallel_tool_calls` — the params keys the proxied OpenAI surface
  adds. The client's own `params` carries only `model`, `messages`, `tools`,
  `system`, `max_tokens`, `stream`, `temperature` and `reasoning_effort`.

## Known differences from the real client

Recorded so they are choices rather than surprises:

- **`promptCache`.** The client puts it on every envelope; this build omits it.
  It is a cache hint with no effect on the response, and the proxy has no
  equivalent state to forward.
- **Lifecycle `mode`.** The client reports `isTTY() ? "interactive" :
  "non-interactive"`; this build reports `interactive` unconditionally, because it
  is a server and a second vocabulary of modes would be one more thing to keep
  consistent.
- **`x-cmd-provider-deepseek-internal`.** Present in the client's header table;
  this proxy does not send it.

## Verified against the live service

Checked with a real key against `https://api.commandcode.ai`. This is what the
literal comparison above cannot see: it proves the client's vocabulary, and the
service's behavior is a separate question.

All four paths exist. The three `/alpha` routes answer only `POST` — a `GET` gets
the service's own 404 — and the catalogue answers `GET`.

| Path | Method | Result with a valid answer |
|---|---|---|
| `/alpha/generate` | POST | 400 on an empty body |
| `/alpha/fingerprint/record` | POST | 200 `{"success":false,"code":"BAD_REQUEST"}` |
| `/alpha/lifecycle-events` | POST | 400 `Invalid event type or metadata` |
| `/provider/v1/models` | GET | 200, 69 models |

### User-Agent is mandatory

Every request to the service must carry a `User-Agent`. Without one the edge
answers `403 error code: 1010` — a Cloudflare rule, not the application — before it
looks at the path. **Any value passes**; what matters is that the header exists.
`cli` is the value the client sends and the value used here.

Every other header was dropped one at a time and the request still succeeded.
`Authorization` is not even required for the catalogue: `/provider/v1/models`
answers 200 without it. That was a real defect before it was checked — `generate`
carried a `User-Agent` and the announcement and catalogue did not, so the
lifecycle pre-flight and the model list both failed against the live service,
and the rewrite had inherited the same omission from `commandcode-proxy`, whose
`ensureInitialized` sends the same header set without one. The sets are now built
by one function so they cannot diverge again.

### Re-checking it

Two checks, because they fail differently. The live service says whether a header
set is accepted; a mock upstream says whether this build actually emits it. The
second is worth having, because a pre-flight that fails is logged and then
ignored — the turn is served anyway, so a wrong header here is invisible in the
response.

Point `api_base` at a local server that records what it receives, send one request
through the proxy, and read back the capture. The announcement arrives as two
requests before the generate:

```
POST /alpha/lifecycle-events
    user-agent               cli
    x-command-code-version   1.53.1
    x-cli-environment        production
    body {"eventType":"cli_session_exists","metadata":{"sessionId":"sess_3b684d31722c0f7c",
          "cliVersion":"1.53.1","mode":"interactive","os":"win32-x64"}}

POST /alpha/fingerprint/record
    user-agent               cli
    body {"thumbmark":"5e1c…","components":{…}}
```

### x-command-code-version is required, with no floor

`/alpha/generate` answers `403 upgrade_required` when `x-command-code-version` is
absent. It is not a minimum-version check: `1.0.0` and `9.9.9` both pass, and only
the absence is rejected. The dialect already sends the header on every request, so
this costs nothing — but it is why the version reported upstream must always be
present and well-formed.
