# Bifrost

Bifrost is a protocol gateway: it translates the private Command Code wire
protocol into three public interfaces — OpenAI Chat Completions, Anthropic
Messages and OpenAI Responses.

It is the Rust rewrite of `commandcode-proxy`. The original was a single 3,000
line file; Bifrost splits the same behavior into testable crates, borrowing the
protocol-conversion contract from `deepseek-recipe` and the mechanism/audit
discipline from `SoL-Pi`.

One account, many harnesses: Claude Code, Codex CLI, an editor plugin and a script
can all be billed to the same `user_…` key without knowing anything about the
Command Code wire protocol.

## Quick start

```sh
cargo build --release -p bifrost-edge --bin bifrost

printf 'version = 1\nhost = "127.0.0.1"\nport = 3050\n' > bifrost.toml
./target/release/bifrost --check --config bifrost.toml   # validates, binds nothing
./target/release/bifrost --config bifrost.toml           # serves

# The key is passed in per request, never configured — the CLI already wrote one.
KEY=$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.commandcode/auth.json")))["apiKey"])')
curl -sS http://127.0.0.1:3050/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"deepseek/deepseek-v4-flash","messages":[{"role":"user","content":"Say OK"}],"max_tokens":16}'
```

## Wiring a client

Any client that speaks OpenAI Chat Completions, Anthropic Messages or OpenAI
Responses, can be aimed at another base URL, and sends the key as a bearer token or
`x-api-key`, works. Two of them are known to work end to end, not merely in
principle:

| Client | What to set |
|---|---|
| Claude Code | `ANTHROPIC_BASE_URL=http://127.0.0.1:3050`, `ANTHROPIC_API_KEY=user_…`, and `ANTHROPIC_MODEL` / `ANTHROPIC_DEFAULT_HAIKU_MODEL` pointing at a model the account has — or a `[models.aliases]` rule, which leaves the client's own defaults alone |
| Codex CLI | a provider with `base_url = "http://127.0.0.1:3050/v1"`, `wire_api = "responses"`, `env_key = "…"` |
| OpenAI SDK | `base_url = http://127.0.0.1:3050/v1` |
| Anthropic SDK | `base_url` plus `x-api-key` or `auth_token` |

The model name is the client's to get right: an account that does not have the
model it is asked for answers `401 MODEL_NOT_IN_PLAN`, and `GET /v1/models` is the
provider's list rather than a plan-filtered one — it names models a plan refuses,
which is exactly what a client asking for `claude-sonnet-5` runs into. Either point
the client at a model that answers, or write a `[models.aliases]` rule that points
the client's own name there. Bifrost rewrites nothing on its own: a name no rule
matches is forwarded as it arrived, and the refusal that comes back is the
upstream's real answer.

The official `cmdc` client is not one of them: it talks the `/alpha/*` protocol, and
Bifrost serves the three public protocols instead. That is the direction of the
translation, not a gap in it.

## Handing it out

Forwarding the caller's key is the default, and it is the right answer for one
machine. A deployment that is more than that — another laptop, a colleague, a fleet
of agents — can hold the key itself and issue tokens instead:

```toml
[access]
enabled = true
key_file = "/home/you/.commandcode/auth.json"
tokens_file = "var/tokens.json"
```

```sh
./target/release/bifrost --token-new laptop --rpm 60 --concurrency 2 --config bifrost.toml
# token `laptop` issued: bfr_9f0c…   — printed once, and only here
```

The token is the client's credential, in the same header as the key ever was. What it
buys is revocation and accounting: the file holds only each token's sha256, so it can
be read and backed up without being a credential; `--token-revoke` and `--token-new`
reach a running deployment without a restart; `--rpm` and `--concurrency` keep one
caller from spending the account for everybody; and each turn's access line records
`token=<name>`, which is the question a shared subscription otherwise cannot answer —
who is using this. The key itself still never leaves the machine it was given on: it
is read from the file named above, never printed, never logged, never sent to a
client. A `user_…` key sent to a deployment that issues tokens is refused: a
credential that still worked would be one revocation does not reach.

## Documentation

| Document | What it holds |
|---|---|
| `docs/usage.md` | the manual: build, configure, run, wire a client, operate, troubleshoot |
| `docs/usage.zh.md` | the same manual in Chinese |
| `docs/architecture.md` | the crates and the contracts between them |
| `docs/wire-alignment.md` | how the dialect is checked against the published client, and the result |
| `bifrost.example.toml` | every setting there is, with its default and the reason for it |

## Why a rewrite

- **Memory.** The original buffered each request body several times over; the
  README measured a peak of 5.1–7.4× the body size, so a 100 MB ceiling could
  cost ~550 MB. Bifrost converts while reading and keeps one canonical copy.
- **Shape.** One file meant protocol conversion, wire encoding, resilience and
  HTTP handling were interleaved. They are now separate crates with explicit
  contracts.
- **Typing.** Every protocol payload is a `serde` type, so a malformed field is
  caught at the boundary instead of during string handling.
- **Drift.** The upstream protocol version was a hard-coded constant. It is now
  a pluggable adapter, so a new dialect is a new implementation rather than an
  edit to the main path.

## Design

```
edge (axum/hyper) ── tower middleware, streaming body limits, backpressure
  │
inbound adapters ── OpenAI Chat · Anthropic Messages · OpenAI Responses
  │                      ↓ into_canonical
canonical IR ────── bifrost-core
  │                      ↓
mechanisms (opt-in) ─ fingerprint · session · prompt cache · archive
  │
upstream adapters ── bifrost-wire: envelope, headers, retry, error mapping
  │                      ↓ upstream stream
stream engine ───── incremental decode → OutputChunk → protocol events
  │                      ↓ from_canonical
outbound adapters ── the same three protocols
```

Adapters never call each other. Every conversion goes through the canonical IR,
which keeps the number of conversion paths linear instead of quadratic.

## Status

Phases P1 through P22 are complete: the gateway runs, every switch it advertises
is honored, `/v1/models` reports the upstream's own catalogue, a deployment is
told when the client its dialect was read from moves on, the machine it claims to
be is configured in one place, every request leaves one line saying what it was and
what it answered, what the archive kept can be read back — listed, verified,
audited, and handed over a turn at a time or a conversation at a time — and a
deployment can hold the account key itself and hand callers tokens of their own,
limited by the minute and by turns at once, revocable without a restart. The dialect
has been checked against the live service as well as against the published client —
see `docs/wire-alignment.md`.

Two harnesses have been driven through a running build rather than argued about:
Claude Code (43k tokens of system prompt and tool definitions) and Codex CLI over
the Responses API, both answering correctly, with the bytes this build puts on the
wire read back from a recording stand-in upstream. `docs/usage.md` records what was
verified and how.

| Crate | What it holds |
|---|---|
| `bifrost-core` | Canonical request/response IR, unified error surface |
| `bifrost-config` | Layered, strictly validated configuration |
| `bifrost-audit` | Content-addressed archive, append-only journal, secret guard |
| `bifrost-fingerprint` | Device identity, byte-compatible with the official CLI |
| `bifrost-wire` | `WireAdapter` contract and the `cc/1.53.1` dialect |
| `bifrost-protocol` | `ProtocolAdapter` contract, OpenAI Chat, Anthropic Messages, OpenAI Responses |
| `bifrost-edge` | axum HTTP surface, admission control, issued tokens and their limits, the streaming forwarding path, the model catalogue, the drift watch |

All three protocols work end to end through a running server: a client body
decodes through the IR into a complete `cc/1.53.1` envelope, the upstream stream
decodes back into that protocol's events, and `bifrost-edge` serves it — routing,
CORS, admission control, body limits, the idle watchdogs and the streaming path.
`cargo run -p bifrost-edge --bin bifrost` starts it.

A streamed turn is read by a task of its own and handed to the client over a
bounded channel, and a client that stops reading is cut off from the upstream
once `limits.client_stall_ms` has passed — the token bill follows the reader, not
the socket.

Every switch under `mechanisms` is honored. `fingerprint`, `lifecycle` and
`session` reproduce the original proxy's per-key identity and default to on; the
three that add work are opt-in:

- `prompt_cache` turns a client's `prompt_cache_key` into a cache breakpoint on
  the last system section, unless the client marked one itself;
- `timeout_context_hint` counts idle timeouts that arrive back to back and words
  the third one as advice to shorten the conversation, rather than as a slow turn;
- `evidence_archive` keeps each turn's original bytes under `audit.archive_dir`
  and appends one line per finished turn to a JSONL journal in
  `audit.journal_dir`.

`lifecycle` is the pre-flight, and honoring it is what makes `fingerprint`
effective rather than merely computed. A generate envelope carries the working
directory and the environment, but the device identity goes nowhere except its own
endpoint — so before this existed, the fingerprint crate was faithful and unused,
and the upstream saw a key with no machine behind it. The pre-flight is `POST
/alpha/fingerprint/record` and `POST /alpha/lifecycle-events`, sent once per key
per window of eight hours plus up to two of jitter, and awaited by the turn that
triggers it so the upstream knows the device before it generates. The window is
claimed before the calls go out, so requests arriving together send one pair
between them; the wait is bounded by `limits.announce_ms`; and a pre-flight that
is refused, fails, or never answers is logged and the turn is served anyway.

That last property is worth knowing when one of these is being debugged: because a
failed announcement never fails a turn, a pre-flight that is being rejected at the
edge leaves no trace in the response. The upstream requires a `User-Agent` on
every request and answers one without it with `403 error code: 1010` before
looking at the path, so the whole announcement — device record and session event —
is silently absent if that header is ever dropped. It is sent, and pinned by test.

The version reported upstream is the one this build implements, and it never
moves on its own. `wire.drift_watch` reads what the client is currently published
as — at startup and once a day, from `wire.drift_registry` — and says so when the
two differ, which is the cue to re-read the package and re-align the dialect. The
check reads a registry and writes a log line; it cannot change a request.

What to run when that line appears is `tools/check-dialect-alignment.sh`: it
compares the vocabulary this dialect implements — paths, header names, body keys,
the lifecycle event, the tool aliases — against the published client, and exits
non-zero only when one of them is gone or has moved. `docs/wire-alignment.md`
records the method and the result of the first run, which was 1.54.0: the dialect
did not need to move.

The archive is the one mechanism that touches disk, and it stays out of the way:
it is written off the async thread, a write that fails is logged rather than
raised, and a stream is kept up to 8 MB and flagged `truncated` past that. A
stream is recorded however it ends — at its own end, or when the client walks away
from it, which is the case worth having evidence for. The journal holds digests,
byte counts, the model, the session and a digest of the key — never the key
itself, and never the bodies, which stay in the archive under the digest that
names them.

The archive grows for as long as the deployment runs, so `audit.retain_days` and
`audit.max_total_mb` are what keeps it to a size: a pass runs at startup and once a
day, removing bytes older than the window and then the oldest bytes over the
ceiling. The journal is never pruned — it is the index, and a line whose bytes are
gone still says what happened — and a blob is kept when a turn inside the window
still names it, because identical bytes are stored once, so an old file can be the
bytes of yesterday's turn. When the ceiling cannot be met without breaking the
window, the pass keeps the window and says so in the log rather than deciding on
its own which evidence mattered. Every pass leaves a `retention` line in the
journal, so a digest with no bytes behind it can be told from a broken writer.

### The model catalogue

`/v1/models` asks the upstream's own catalogue path, because that is what the
provider offers, rather than what the build was tested against. It is not a list of
what this account is entitled to: the upstream includes models a plan does not
cover, and those answer `401 MODEL_NOT_IN_PLAN` when asked for — a Go plan in this
repository's checks lists `claude-sonnet-5` and is refused it. The answer is cached for `models.refresh_ms` and shared by the whole
deployment, and a fetch that is already in flight is not started a second time,
so a fleet of clients starting together sends one request between them. A
catalogue is a read of what the upstream serves and does not pass through the
IR.

A failed refresh does not fall back to the compiled-in table; it serves the last
catalogue the upstream gave. That is a deliberate difference from the original,
which resets to its hard-coded list whenever a fetch fails and so downgrades a
transient upstream error into a memory of what the models were at build time. A
list the upstream produced is evidence of what it serves, so it is kept until it
can be replaced. The built-in table is for a deployment that has never had an
answer: a first start with the catalogue unreachable, or `models.provider`
turned off. With the switch off, and for a caller that has not presented a key,
no upstream call is made at all.

### Golden fixtures

The protocol and fingerprint crates are diffed against the original JavaScript,
not against a reading of it. Each crate keeps the vectors it is checked with under
`fixtures/`, produced by running regions of `commandcode-proxy/proxy.mjs`
verbatim; `bifrost-protocol/fixtures/README.md` explains how to regenerate them
and why editing an expectation is a breaking change.

## Running it

`cargo build --release -p bifrost-edge --bin bifrost` produces the binary;
started with no arguments it serves. It reads `$BIFROST_CONFIG`, or
`./bifrost.toml` when there is one, and then the environment —
`bifrost.example.toml` names every key there is.

Flags exist for a deployment rather than for a developer:

- `bifrost --check` reads and validates what it would start with, names the setting
  to fix when it cannot be started with, and exits non-zero. It binds nothing and
  writes nothing, which is what makes it usable ahead of the service it belongs to.
- `bifrost --print-config` prints the configuration the process resolved to, with
  the fingerprint salt replaced by `<redacted>`. The file someone edited is one of
  three layers, so what the process actually resolved to is the answer that was not
  already on their screen; what it prints is a configuration, and can be read back
  as one.
- `--config PATH` names the file directly, winning over `$BIFROST_CONFIG` and over
  `./bifrost.toml`, so a candidate file can be checked before it is installed.

Four more read back what the archive mechanism wrote, in the same configuration the
server runs with, so the answer is about the deployment rather than about whichever
directory the operator happened to be standing in:

- `bifrost --journal` prints the decision journal, oldest entry first, as the lines
  it stores — the command filters and never reformats, because the file is one an
  operator greps. `--kind` narrows it to one kind of entry, `--since` to entries at
  or after an RFC 3339 timestamp (an entry that cannot be dated is shown either
  way), `--session` to one conversation — an equality, so a line that names no
  session is not a line about that one — and `--limit` to the newest few.
- `bifrost --verify DIGEST` looks a digest up in the archive: `0` when the bytes are
  there and still hash to it, `1` when they do not (`tampered`) or when a `--quote`
  is not in them (`absent`), and `3` when nothing is archived under that digest
  (`gone`) — which is what retention leaves behind, and is a different finding from a
  claim that did not hold.
- `bifrost --audit` asks that question of everything the journal names at once, which is
  the shape of the question an operator has after changing retention: one line of counts
  (`entries N, named N, intact N, tampered N, gone N, malformed N`, counted per digest,
  since identical bytes are stored once) and then one line per finding. It takes the
  same `--since` and `--kind` filters as `--journal`, and answers with `--verify`'s
  codes and meanings: `0` when every digest named is intact, `1` when a blob no longer
  hashes to its name or a name is not a digest, and `3` when the only finding is bytes
  retention removed.
- `bifrost --turn DIGEST` hands the bytes over: the turn's journal line as it is stored,
  then, for each half of that turn, what `--verify` says about its digest followed by the
  bytes themselves. They are written out as they are stored — a body is whatever the
  client sent, so it is not necessarily text — and a newline follows one only when it does
  not end in one, which keeps the next line a line of its own. Identical bytes are stored
  once, so every line that names the digest is handed over. Given `--session` instead of a
  digest, every turn of that conversation is handed over, oldest first — a session names a
  conversation, and a conversation is several turns. `0` when the halves named are intact,
  `1` when one no longer hashes to its name, `3` when one is gone or was never archived.

`deploy/bifrost.service` runs it under systemd as an unprivileged user with its own
state directory and a sandbox that grants it one socket, one outbound connection
and nowhere to write but that directory — the relative `var/journal` and
`var/archive` of the default configuration land inside it. Both commands the unit
runs name the file with `--config /etc/bifrost/bifrost.toml`, and the check is wired
into `ExecStartPre`, so a configuration this build cannot use fails the unit instead
of the first request that needs it, and the file that was checked is the file that
is served. `deploy/bifrost.env.example` is the environment
file the unit reads, and every name in it is one the original proxy already used,
so an existing deployment does not need a new set of secrets. The install steps are
in the unit's own header.

A stop is answered on either signal: a manager's SIGTERM and a terminal's SIGINT
both stop the accept loop, let the requests in flight finish and leave a line in
the log saying so. What the unit gets wrong is only what it was not told — a
streamed turn can run past `TimeoutStopSec`, at which point systemd cuts it.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

`systemd-analyze verify deploy/bifrost.service` checks the unit itself. Before the
binary is installed it complains about the path it cannot find and nothing else,
which is the shape of a unit whose directives are all accepted.

Two checks live outside the test suite, because neither question can be asked of a
unit test. `tools/check-dialect-alignment.sh` compares the dialect's vocabulary
against the published client and says whether a drift line means the shape moved.
`tools/smoke.sh` starts a build and talks to the live service — health, the model
catalogue, and with `--generate` one real turn — then reads the server's own log
back: the access line each request left, the warnings a refused pre-flight shows up
in and nowhere else, a check that the key never reached the log, and a stop signal —
the one a service manager sends — answered with a clean exit and a line saying so.
Both can prove they are capable of failing: `CC_SELFTEST=1` for the first,
`--self-test` for the second.

Copies `bifrost.example.toml` to `bifrost.toml` to configure a deployment. Every
key is optional; unknown keys are rejected.

The device profile is built from `[device] project_dir`, the original's
`CC_DEVICE_PROJECT_DIR`: the working directory the upstream sees, the project slug
derived from it, and the machine the fingerprint describes all follow it, because
a combination of those that disagreed is one no real client produces.

Everything the edge reports goes through one logger: `telemetry.log_level` filters
it and `telemetry.log_format` chooses one JSON object per line or one plain line
per event. Every request gets a line of its own, including the ones that reached no
endpoint, and it carries what the turn knew when it answered — the method, the path,
the status, the time it took to begin, and then the protocol, the model, the stream
flag and the fingerprint of the key it was billed to, once it had got that far. A
turn a rule pointed at another model records both: `model` is what answered, and
`requested_model` is what the client asked for. The
key itself never appears; a log is read by more people than the key is given to.
`GET /status` answers what this process has been doing, as counts: how long it has
been up, how many turns it answered, and how many requests it refused and for which
reason. That is how an operator tells "nothing is arriving" from "nothing is
working" without reading a log. It is aggregate and it needs no key, so nothing in
it names a key, a model or a body, and no decision anywhere is taken from a number
in it. There is no metrics endpoint — the setting that suggested one was removed
rather than left unread, because a switch that turns nothing on is a promise the
deployment cannot keep.

The edge needs an HTTP stack and TLS to reach the upstream, so this crate carries
`axum`, `tokio` and `reqwest` (rustls). Everything below it is still dependency-free
apart from `serde`, `toml`, `sha2` and `time`, and `Cargo.lock` pins the whole tree
so an offline build keeps working once the sources are cached.

## License

MIT — see `LICENSE`.
