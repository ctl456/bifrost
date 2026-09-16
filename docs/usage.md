# Using Bifrost

This is the operational manual: how to build it, what to put in the file, how to
run it, how to point a client at it, and what to do when an answer is not the one
you expected. The Chinese version is `docs/usage.zh.md`; this one is the original,
so a change lands here first. `README.md` explains why the design is shaped the way it is;
`docs/architecture.md` explains the crates.

## What it is

Bifrost is a **reverse proxy in front of one Command Code account**. A client that
speaks OpenAI Chat Completions, Anthropic Messages or OpenAI Responses talks to
Bifrost; Bifrost talks the private `cc/1.53.1` wire protocol to
`api.commandcode.ai` on that client's behalf.

It is what `commandcode-proxy` did, rewritten in Rust: one account, many
harnesses. Claude Code, Codex CLI, an editor plugin, a script — each one keeps
speaking its own protocol, and each one is billed to the same `user_…` key.

## What it is not

- It does not create or share accounts. Every client needs a `user_…` key that the
  account already has; what the account's plan allows is what the account gets.
- It does not accept the CLI's own protocol as input. The official `cmdc` client
  talks `/alpha/*`, and Bifrost serves the three public protocols instead. Pointing
  `cmdc` at it answers `404`; that is by design, not a bug to work around.
- It does not multiplex several upstream accounts behind one endpoint.

## Requirements

- Rust, pinned by `rust-toolchain.toml`. Nothing else: no Node, no Docker.
- Network access to `api.commandcode.ai` from wherever it runs.
- A key. `cmdc login` writes one to `~/.commandcode/auth.json`; Bifrost does not
  read that file — you pass the key in, on each request, as a client would.

## Build

```sh
cargo build --release -p bifrost-edge --bin bifrost   # -> target/release/bifrost
```

## First run

```sh
# 1. A configuration. Every key is optional; this is the whole minimum.
cat > bifrost.toml <<'TOML'
version = 1
host = "127.0.0.1"
port = 3050
api_base = "https://api.commandcode.ai"
TOML

# 2. Check it before trusting it. Binds nothing, writes nothing.
./target/release/bifrost --check --config bifrost.toml

# 3. Serve.
./target/release/bifrost --config bifrost.toml
```

Then, in another shell, with the key from `~/.commandcode/auth.json`:

```sh
KEY=$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.commandcode/auth.json")))["apiKey"])')

curl -sS http://127.0.0.1:3050/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"deepseek/deepseek-v4-flash","messages":[{"role":"user","content":"Say OK"}],"max_tokens":16}'
```

`GET /health` answers `OK` and needs no key, which is what to check first when a
client says it cannot reach anything.

## Configuration

The configuration is built in layers, each one overriding the one before:
defaults, then the file, then the environment.

| Where | How |
|---|---|
| A named file | `--config PATH` — wins over everything else |
| The conventional file | `$BIFROST_CONFIG`, or `./bifrost.toml` when that is unset and the file exists |
| The environment | The variables below, applied after the file |

Unknown keys are rejected rather than ignored: a typo is a failed start with the
offending key named, not a setting that silently did nothing.

The environment variable names are the ones `commandcode-proxy` already used, so an
existing deployment needs no new secrets:

| Variable | Sets |
|---|---|
| `BIFROST_CONFIG` | which file to read |
| `PORT`, `HOST` | listen address |
| `CC_API_BASE` | upstream base URL |
| `CC_DEVICE_PROJECT_DIR` | the working directory reported upstream |
| `CC_FINGERPRINT_SALT` | rotates the derived device identity in bulk |
| `CC_USE_PROVIDER_MODELS` | `false` turns the upstream catalogue off |
| `CMD_ZDR` | `1`/`true`/`yes` asks for zero-data-retention routing |
| `BIFROST_LOG_LEVEL` | `error`, `warn`, `info` or `debug` |

`bifrost.example.toml` documents every setting there is, with the default and the
reason for it. `--print-config` prints what the process actually resolved to, with
the fingerprint salt redacted — the file you edited is one of three layers, so the
resolved value is the answer that was not already on your screen.

### The key arrives per request — unless this deployment issues tokens

By default the key arrives per request, as a client's would, in
`Authorization: Bearer …` or `x-api-key: …`, and both headers are accepted on every
endpoint. Bifrost does not read `~/.commandcode/auth.json` and has no setting for a
key, because a key in a configuration file is a key in a backup, a unit file and a
`ps` output. Under systemd the client-facing key is the client's problem; the unit
needs none.

`[access]` is the other arrangement, for a deployment that is not only your own
machine: the key is read from a file this process owns and every caller is given a
token instead. Nothing about the default changes while it is off, and it is off.

```toml
[access]
enabled = true
key_file = "/home/you/.commandcode/auth.json"
tokens_file = "var/tokens.json"
```

### Issuing a token

```sh
./target/release/bifrost --token-new laptop --rpm 60 --concurrency 2 --config bifrost.toml
# token `laptop` issued: bfr_9f0c…   — shown here once, and only here
./target/release/bifrost --token-list              --config bifrost.toml
./target/release/bifrost --token-revoke phone      --config bifrost.toml
```

- The token is the client's credential in place of the key: `Authorization: Bearer
  bfr_…` or `x-api-key: bfr_…`. Nothing else about a client's configuration changes.
- `--token-new` prints it once and keeps its sha256, so a lost token is issued again
  rather than looked up, and the file can be read, copied and backed up without being
  a credential. It is written `0600`.
- A name is one word — letters, digits, `-`, `_`, `.` — because it ends up in an
  access line as `token=<name>` and is the argument to `--token-revoke`. A name that
  has been used stays taken: a revoked token is marked rather than deleted, so a name
  seen in a log can be told from one that was never issued.
- `--rpm` and `--concurrency` are per token and default to no limit. The minute is a
  bucket that refills continuously, so a caller at the limit waits the fraction of a
  minute its next request is worth instead of a wall-clock boundary; the concurrency
  ceiling is what keeps one caller from holding every place this deployment has.
- The serving process re-reads the file when it changes, so issuing and revoking reach
  a running deployment. Deleting the file is a deployment with no tokens: that refuses
  everything, which is the loudest thing an accidental deletion can do.
- A key is not a token here. A `user_…` key sent to a deployment that issues tokens is
  refused with a `401` that says so, and that is the point: a credential that still
  worked would be one revocation does not reach.
- `GET /status` gains a row per token — name, requests served, in flight, idle time,
  revoked — which is the question the aggregate counters cannot answer: which caller
  is the one filling the ceiling. That row is also why this page is the one surface
  that stops being anonymous here: it takes the token a turn takes, or answers `401`.
  A deployment that forwards its callers' keys names nobody in it and is still read
  without a credential, and `/health` needs nothing in either shape.
- **The key is still not in the file.** `access.key_file` names a file this process
  reads; the key itself is never printed by `--print-config`, never logged, and never
  sent to a client.

## Wiring a client

Any client that (a) speaks one of the three protocols, (b) can be aimed at another
base URL, and (c) can send the key as a bearer token or `x-api-key`, will work.
The `user_…` token is what matters in the header; prefixes around it are tolerated.

| Client | What to set |
|---|---|
| Claude Code | `ANTHROPIC_BASE_URL=http://127.0.0.1:3050`, `ANTHROPIC_API_KEY=user_…`, and `ANTHROPIC_MODEL` / `ANTHROPIC_DEFAULT_HAIKU_MODEL` pointing at a model the account has — or a rule, which leaves the client's defaults alone |
| Codex CLI | a provider with `base_url = "http://127.0.0.1:3050/v1"`, `wire_api = "responses"`, `env_key = "…"` |
| Anthropic SDK | `base_url` / `ANTHROPIC_BASE_URL` plus `x-api-key` or `auth_token` |
| OpenAI SDK | `base_url = http://127.0.0.1:3050/v1` |
| Editor plugins (Cline, Roo, Continue, …) | "OpenAI compatible" provider, base URL `…/v1`, model from `/v1/models` |

Two things decide whether a client works where its protocol suggests it should:

- **The model name.** Bifrost forwards the model the client names. A client whose
  default is `claude-sonnet-5` will ask for `claude-sonnet-5`, and the account will
  answer `401 MODEL_NOT_IN_PLAN` if that model is not in the plan. `GET /v1/models`
  is the provider's list, not a plan-filtered one — it names models this plan
  refuses — so either point the client's model setting at something that answers,
  or write a rule and leave the client's own defaults alone.
- **The upstream is streaming-only.** For every client, Bifrost asks the upstream
  for a stream and assembles a whole body itself when the client did not ask for
  one. Nothing about the client's `stream` flag reaches upstream.

### Pointing a model name somewhere else

Some clients cannot be told what to ask for, or ask under a name that moves with
every release of the tool. A rule in `[models.aliases]` maps the name they ask for
onto one this account may use:

```toml
[models.aliases]
"claude-" = "deepseek/deepseek-v4-flash"
"claude-sonnet-5" = "deepseek/deepseek-v4-pro"
```

- The pattern is a prefix, compared without regard to case, and the longest pattern
  that matches wins: `claude-sonnet-5` is served by the second rule above and
  `claude-opus-4-6` by the first. An exact name is just the longest pattern that can
  match itself, so it needs no special case.
- A name no rule matches is forwarded as the client spelled it, so a typo stays the
  upstream's `401 MODEL_NOT_IN_PLAN` instead of becoming another model's answer.
  There is no catch-all and no default model: a rule table is not a fallback.
- The rewrite happens before the request is encoded, which is what keeps the three
  names involved in agreement: the upstream is asked for the new name, the response
  reports that same name, and the access line records both of them — the name that
  answered as `model=`, the name that was asked for as `requested_model=`.
- The request bytes archived under `evidence_archive` are the client's own, so the
  name it really sent is still there if a rule is ever in question.
- `GET /v1/models` is unaffected: it answers what the provider offers, which is more
  than this plan may use, and a rule does not add to it.
- A rule with an empty pattern, or one that names no model, refuses to load: a rule
  that matched every name while looking like a single entry is a typo nobody would
  find by reading it.

### Endpoints

| Method | Path | Key? | Answers |
|---|---|---|---|
| `GET` | `/health`, `/` | no | `OK` — liveness only |
| `GET` | `/status` | where it names callers | Counters: uptime, turns, refused, unauthenticated, too_large, malformed, upstream_failed, timeouts, client_stalls, inflight, max_inflight, and one row per issued token — names, never credentials. The rows are why this one takes a token when the deployment issues them |
| `GET` | `/v1/models` | no | The provider's catalogue, cached; the built-in table only before the first successful fetch. Not plan-filtered: it names models this plan refuses. A deployment that issues tokens fetches it with the key it holds, because a catalogue is the deployment's own question rather than a caller's |
| `POST` | `/v1/chat/completions` | yes | OpenAI Chat Completions, streaming and whole |
| `POST` | `/v1/messages` | yes | Anthropic Messages |
| `POST` | `/v1/responses` | yes | OpenAI Responses |

## Operating it

Everything it says goes to one log: one JSON object per line by default, or one
plain line per event with `log_format = "text"`. Every request leaves a line — the
method, path, status, time to first byte, and then the protocol, model, stream flag
and key fingerprint once those are known. A turn a rule pointed at another model
records both names — `model` is what answered, `requested_model` is what was asked
for. **The key itself is never logged.**

`GET /status` answers what this process has been doing, as counts, with no key
needed — unless the deployment issues tokens, in which case the body names them and
it takes the same token a turn does: that is how "nothing is arriving" is told from
"nothing is working", and a page that names callers is not a page to hand to whoever
can reach the port.

With `evidence_archive = true`, each turn's original bytes are kept and one line per
turn is appended to the journal. Five flags read that back, using the same
configuration the server uses — under the shipped unit that means passing
`--config /etc/bifrost/bifrost.toml`, because that is how the unit names it:

| Command | Question it answers |
|---|---|
| `--journal` | what happened, in order (`--kind`, `--since`, `--session`, `--limit` narrow it) |
| `--verify DIGEST` | are these bytes still the bytes that were stored (`--quote` also looks for a string) |
| `--audit` | all of that at once: one line of counts, then one line per finding |
| `--turn DIGEST` | hand over a turn: its journal line, then each half's bytes |
| `--turn --session ID` | hand over a whole conversation, oldest turn first |

Exit codes are the same for `--verify` and `--audit`: `0` intact, `1` a blob no
longer hashes to its name (or a name is not a digest), `3` the bytes are gone —
which is what a retention pass leaves behind, and a different finding from a claim
that failed.

Retention is `audit.retain_days` and `audit.max_total_mb`, applied at startup and
once a day. The journal is never pruned; a blob still named by a turn inside the
window is kept; when the ceiling and the window disagree, the window wins and the
log says so.

Under systemd, `deploy/bifrost.service` runs it unprivileged with its own state
directory, one socket, one outbound connection and nowhere else to write. Both
commands the unit runs name the configuration with `--config
/etc/bifrost/bifrost.toml`, and `--check` is wired into `ExecStartPre` so a bad file
fails the unit rather than the first request — and so that the file validated is the
file served. `deploy/bifrost.env.example` is the environment file the unit reads; it
cannot move the configuration, because a path named on the command line wins over
`BIFROST_CONFIG`. Install steps are in the unit's own header.

## Troubleshooting

| Symptom | What it is |
|---|---|
| `Missing API key` | no `user_…` token in `Authorization: Bearer` or `x-api-key` |
| `401 MODEL_NOT_IN_PLAN` | the model is real but not in this account's plan; the error comes from the upstream, in the client's own error shape. Pick one from `/v1/models`, or write a rule that points this name at one of them |
| A client hangs, then times out | a reasoning model thinking before its first token. `/v1/messages` sends comment-frame heartbeats for exactly this; a client that cannot tolerate them will still time out |
| A pre-flight is being refused | it never fails a turn, so it leaves no trace in any response — look at the log for the warning, and remember a missing `User-Agent` gets a `403` before the path is even read |
| `--verify` answers `3` | retention removed those bytes; the journal line still says what happened |
| A drift line in the log | the published client moved ahead of `cc/1.53.1`. Run `tools/check-dialect-alignment.sh`; the dialect does not move on its own |
| Config will not load | run `--check`; it names the setting. Unknown keys are rejected on purpose |

## Checks

```sh
cargo test --workspace                                     # the whole gate
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
tools/check-dialect-alignment.sh                           # vocabulary vs the published client
tools/smoke.sh --generate                                  # a build, a running server, one real turn
```

The first three run on every push and pull request, from
`.github/workflows/ci.yml`. The dialect check is that file's other job and runs on
a schedule: what it reports is that the published client moved, which is true of
every branch at the same moment and is not something a change decides.

`tools/smoke.sh` starts a build and talks to the live service, then reads the
server's log back: the access line each request left, the warnings a refused
pre-flight shows up in, that the key never reached the log, and a stop signal
answered with a clean exit. Both external checks can prove they are able to fail —
`--self-test` for the smoke script, `CC_SELFTEST=1` for the alignment script.

## What has been verified

Protocol shapes and the fingerprint are diffed against the original JavaScript, not
against a reading of it: each crate keeps the vectors it is checked with under
`fixtures/`, produced by running regions of `commandcode-proxy/proxy.mjs` verbatim.
`docs/wire-alignment.md` records the first run of the alignment check, which was
against client `1.54.0`: the `cc/1.53.1` dialect did not need to move.

End to end, against the live service:

- the three protocols, streaming and whole, with tools, images, `reasoning_effort`
  and prompt caching;
- the wire actually sent, read back from a recording stand-in upstream: the
  pre-flight pair, the envelope's key order, tool definitions as `input_schema`,
  images as `{"type":"image","image":"data:…","mimeType":…}`, and headers
  (`user-agent: cli`, `x-command-code-version`, `x-project-slug`, `traceparent`);
- Claude Code `2.1.119` (43k-token system prompt and tools) and Codex CLI `0.115.0`
  (Responses API) driven through it, both answering `OK`;
- a deployment that issues tokens: a token serves a turn while the upstream is spoken
  to with the deployment's own key, a `user_…` key is refused where tokens are issued,
  a revocation and a fresh issue both reach a running process, an empty minute answers
  `429` with the wait, and a token over its ceiling answers the same retryable `503`
  this deployment's own ceiling does.

## Deliberate differences from the original

- A failed catalogue refresh keeps the last catalogue the upstream gave instead of
  resetting to the compiled-in table.
- The lifecycle mode is reported as `interactive` and never as `non-interactive`;
  nothing else in the envelope is configurable either.
- There is no metrics endpoint, and no switch that turns nothing on.
