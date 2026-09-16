# Bifrost

Bifrost is a reverse proxy in front of one Command Code account. It accepts the
OpenAI Chat Completions, Anthropic Messages and OpenAI Responses protocols, and
speaks Command Code's own `cc/1.53.1` wire dialect upstream. One account, many
harnesses: Claude Code, Codex CLI, an editor plugin and a script can all be billed
to the same key without knowing anything about the wire protocol.

It is a Rust rewrite of `commandcode-proxy`, keeping that project's two contracts —
the protocol conversions, byte for byte, and the per-key device identity — and
adding the things a single 3,000-line file could not have: a typed IR between the
protocols, a streaming path with real backpressure, and issued tokens for a
deployment that is more than one machine.

## Quick start

```sh
cargo build --release --bin bifrost

printf 'version = 1\nhost = "127.0.0.1"\nport = 3050\n' > bifrost.toml
./target/release/bifrost --check --config bifrost.toml   # validates, binds nothing
./target/release/bifrost --config bifrost.toml           # serves

# The key is passed in per request, never configured — the CLI already wrote one.
KEY=$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.commandcode/auth.json")))["apiKey"])')
curl -sS http://127.0.0.1:3050/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"deepseek/deepseek-v4-flash","messages":[{"role":"user","content":"Say OK"}],"max_tokens":16}'
```

`GET /health` answers `OK` and needs no key, which is the first thing to check when
a client says it cannot reach anything.

## Wiring a client

Any client that speaks one of the three protocols, can be aimed at another base
URL, and sends the key as a bearer token or `x-api-key`, works.

| Client | What to set |
|---|---|
| Claude Code | `ANTHROPIC_BASE_URL=http://127.0.0.1:3050`, `ANTHROPIC_API_KEY=user_…`, and `ANTHROPIC_MODEL` / `ANTHROPIC_DEFAULT_HAIKU_MODEL` pointing at a model the account has — or a `[models.aliases]` rule, which leaves the client's own defaults alone |
| Codex CLI | a provider with `base_url = "http://127.0.0.1:3050/v1"`, `wire_api = "responses"`, `env_key = "…"` |
| OpenAI SDK | `base_url = http://127.0.0.1:3050/v1` |
| Anthropic SDK | `base_url` plus `x-api-key` or `auth_token` |

The model name is the client's to get right. `GET /v1/models` answers the provider's
own list rather than a plan-filtered one, so it names models this plan refuses; an
account asked for one answers `401 MODEL_NOT_IN_PLAN`. Point the client at a model
that answers, or write a rule that points its name there. Bifrost rewrites nothing on
its own: a name no rule matches is forwarded as it arrived.

The official `cmdc` client is not a client of this: it talks `/alpha/*`, and Bifrost
serves the three public protocols instead.

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

The token is the client's credential, in the same header the key ever was. What it
buys is revocation and accounting: the file holds only each token's sha256, so it can
be read and backed up without being a credential; `--token-new` and `--token-revoke`
reach a running deployment without a restart; `--rpm` and `--concurrency` keep one
caller from spending the account for everybody; and each turn's access line records
`token=<name>`, which is the question a shared subscription otherwise cannot answer.
The key never leaves the machine it was given on, and a `user_…` key sent to a
deployment that issues tokens is refused, because a credential that still worked
would be one revocation does not reach.

## Documentation

| Document | What it holds |
|---|---|
| `docs/usage.md` | the manual: build, configure, run, wire a client, operate, troubleshoot |
| `docs/usage.zh.md` | the same manual in Chinese |
| `docs/architecture.md` | the modules and the contracts between them |
| `bifrost.example.toml` | every setting there is, with its default and the reason for it |

## Design

```
edge (axum/hyper) ── routing, body limits, admission control, streaming
  │
protocol ── OpenAI Chat · Anthropic Messages · OpenAI Responses
  │              ↓ into_canonical
core ──────── canonical IR ── the only vocabulary the protocols share
  │              ↓ encode
wire ──────── cc/1.53.1 envelope, headers, lifecycle
  │
Command Code API
```

One crate, `bifrost`, split into modules along that picture: `core` holds the
request and response types every protocol is translated into, so no adapter knows
another adapter's schema; `protocol` and `wire` are the two ends of the same
conversation; `fingerprint` is the device identity it carries; `config` is what the
operator writes; and `edge` is everything that knows about HTTP — routing, limits,
logging, issued tokens. Nothing below `edge` knows about HTTP.

Three inbound protocols and three outbound ones is nine pairs. Routing every
conversion through one IR makes it three decoders and three encoders, and a behavior
fixed once is fixed everywhere. The same argument makes the wire dialect an adapter
trait rather than a constant: a new dialect is a new implementation instead of an
edit to the main path, the version reported upstream is always the one this build
implements, and `wire.drift_watch` says when the published client has moved past it.
A drift line points at `docs/usage.md`, which says what to do about it.

The upstream is streaming-only. For every client, Bifrost asks for a stream and
assembles a whole body itself when the client did not ask for one, so a non-streaming
client is a streaming turn with the events put back together. The stream is read by a
task of its own and handed over a bounded channel, and a client that stops reading is
cut off from the upstream once `limits.client_stall_ms` has passed — the token bill
follows the reader, not the socket.

## Status

All three protocols work end to end through a running server: a client body decodes
through the IR into a complete `cc/1.53.1` envelope, the upstream stream decodes back
into that protocol's events, and the edge serves it with routing, admission control,
body limits, the idle watchdogs and the streaming path. Two harnesses have been
driven through a running build rather than argued about — Claude Code (43k tokens of
system prompt and tool definitions) and Codex CLI over the Responses API — and the
bytes this build puts on the wire have been read back from a recording stand-in
upstream. `docs/usage.md` records what was verified and how.

Protocol shapes and the device identity are diffed against the original JavaScript
rather than against a reading of it: `fixtures/` holds vectors produced by running
regions of `commandcode-proxy/proxy.mjs` verbatim, and one diverging byte in any
layer fails a test.

## Running it

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

Those three are what a green mark in CI means, with `--locked`. Two checks live
outside the test suite, because neither question can be asked of a unit test:
`tools/smoke.sh` starts a build and talks to the live service — health, the counts,
the model catalogue, the access line — and with `--generate` one real turn; then it
starts a second deployment that issues tokens and asks it the questions only a
running process can answer. `--self-test` points it at a dead port so the check can
prove it is able to fail.

`deploy/bifrost.service` runs the binary under systemd as an unprivileged user with
its own state directory and a sandbox that grants it one socket, one outbound
connection and nowhere to write but that directory. `--check` is wired into
`ExecStartPre`, so a configuration this build cannot use fails the unit instead of
the first request. Install steps are in the unit's own header.

`Dockerfile` builds the same binary into an image that runs on the defaults a container
wants — `0.0.0.0:3050`, the caller's key forwarded per request — with the entrypoint
set to the binary, so `docker run … bifrost --token-new laptop` is the command line
this build already has. `docker-compose.yml` is the unit file's equivalent: the same
unprivileged account, no capabilities, a read-only filesystem and one writable volume
for `var/tokens.json`. `docker-compose.access.yml` is the overlay that turns the
key-holding arrangement on: a configuration to read, and the key mounted where that
configuration names it. `.github/workflows/docker.yml` builds the image on every pull
request and publishes it to GHCR on a `v*` tag; the commands are in `docs/usage.md`.

## License

MIT — see `LICENSE`.
