# Using Bifrost

The operational manual: how to build it, what to put in the file, how to run it,
how to point a client at it, and what to do when an answer is not the one you
expected. The Chinese version is `docs/usage.zh.md`. `README.md` is the overview;
`docs/architecture.md` explains the modules.

## What it is

Bifrost is a **reverse proxy in front of one Command Code account**. A client that
speaks OpenAI Chat Completions, Anthropic Messages or OpenAI Responses talks to
Bifrost; Bifrost talks the private `cc/1.53.1` wire protocol to
`api.commandcode.ai` on that client's behalf.

It is what `commandcode-proxy` did, rewritten in Rust: one account, many harnesses.
Claude Code, Codex CLI, an editor plugin, a script — each one keeps speaking its own
protocol, and each one is billed to the same key.

## What it is not

- It does not create or share accounts. Every client needs a `user_…` key the
  account already has; what the plan allows is what the account gets.
- It does not accept the CLI's own protocol as input. The official `cmdc` client
  talks `/alpha/*`, and Bifrost serves the three public protocols instead; pointing
  `cmdc` at it answers `404`, by design.
- It does not multiplex several upstream accounts behind one endpoint.

## Requirements

- Rust, pinned by `rust-toolchain.toml` — or the published image, which needs no
  toolchain at all (see "In a container" below). Nothing else: no Node.
- Network access to `api.commandcode.ai` from wherever it runs.
- A key. `cmdc login` writes one to `~/.commandcode/auth.json`; Bifrost does not read
  that file unless `[access]` names it — by default you pass the key in, on each
  request, as a client would.

## Build

```sh
cargo build --release --bin bifrost                    # -> target/release/bifrost
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

The configuration is built in layers, each one overriding the one before: defaults,
then the file, then the environment.

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
`ps` output.

`[access]` is the other arrangement, for a deployment that is not only your own
machine: the key is read from a file this process owns and every caller is given a
token instead.

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
./target/release/bifrost --token-list           --config bifrost.toml
./target/release/bifrost --token-revoke phone   --config bifrost.toml
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
  a running deployment. Deleting the file is a deployment with no tokens, which
  refuses everything — the loudest thing an accidental deletion can do.
- A key is not a token here. A `user_…` key sent to a deployment that issues tokens is
  refused with a `401` that says so: a credential that still worked would be one
  revocation does not reach.
- `GET /status` gains a row per token — name, requests served, in flight, idle time,
  revoked, and what that caller's turns did to the provider's cache — which is the
  question the aggregate counters cannot answer: which caller is filling the ceiling,
  and which one is breaking the prompt cache for everybody. That row is also why this
  page is the one surface that stops being anonymous here: it takes the token a turn
  takes, or answers `401`. A deployment that forwards its callers' keys names nobody
  in it and is still read without a credential, and `/health` needs nothing in either
  shape.
- **The key is still not in the file.** `access.key_file` names a file this process
  reads; the key is never printed by `--print-config`, never logged, and never sent
  to a client.

## Endpoints

| Endpoint | What it is |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions |
| `POST /v1/messages` | Anthropic Messages |
| `POST /v1/responses` | OpenAI Responses |
| `GET /v1/models` | the upstream's own catalogue |
| `GET /status` | this process's own counters |
| `GET /health`, `GET /` | liveness, no credential |

## Wiring a client

Any client that (a) speaks one of the three protocols, (b) can be aimed at another
base URL, and (c) can send the key as a bearer token or `x-api-key`, will work. The
`user_…` token is what matters in the header; prefixes around it are tolerated.

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
  refuses — so either point the client's model setting at something that answers, or
  write a rule and leave the client's own defaults alone.
- **The upstream is streaming-only.** For every client, Bifrost asks the upstream for
  a stream and assembles a whole body itself when the client did not ask for one.
  Nothing about the client's `stream` flag reaches upstream.

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
- The rewrite happens before the request is encoded, which is what keeps the names
  involved in agreement: the upstream is asked for the new name, the response reports
  that same name, and the access line records both of them — the name that answered as
  `model=`, the name that was asked for as `requested_model=`.

## Operating it

Every request leaves one line, including the ones that reached no endpoint. In JSON
it is one object per line, carrying the method, path, status, the time it took to
begin, and then — once the turn had got that far — the protocol, the model, the
stream flag, the session and the fingerprint of the key it was billed to. A turn a
rule pointed at another model records both names. The key itself never appears; a log
is read by more people than the key is given to.

`GET /status` is the same question answered as counters: how long the process has
been up, and how many turns it answered, refused, or failed upstream. That is how an
operator tells "nothing is arriving" from "nothing is working" without reading a log.
Nothing in it quotes a key, a model or a body, and no decision is taken from a number
in it.

It also carries `cache`, summed over every turn this process served: `prompt_tokens`,
the `cached_tokens` the provider served from its cache, the `cache_write_tokens` it
wrote, and `completion_tokens`. `cache_hit_rate` is `cached_tokens / prompt_tokens`,
rounded to four places, and is `null` until a turn has been counted — a rate over no
tokens is unmeasured rather than zero, and a reader that branched on `0.0` would draw
the wrong conclusion from a page that had not yet looked. This is the one number a
client cannot work out for itself: a harness knows its own token estimate, not what
the provider's cache did with the prompt. A prefix that stopped being stable shows up
here as a rate that falls while `cache_write_tokens` rises, which is the signal an
operator wants before the bill explains it. Counts, not money: no price list is read
anywhere in this process.

`--print-config` is the third way to see what a deployment is: it prints the resolved
configuration with secrets redacted, which is the answer that was not already in the
file you edited.

Under systemd, `deploy/bifrost.service` runs it unprivileged with its own state
directory, one socket, one outbound connection and nowhere else to write. Both
commands the unit runs name the configuration with
`--config /etc/bifrost/bifrost.toml`, and `--check` is wired into `ExecStartPre` so a
bad file fails the unit rather than the first request — and so that the file validated
is the file served. `deploy/bifrost.env.example` is the environment file the unit
reads. Install steps are in the unit's own header.

A stop is answered on either signal: a manager's SIGTERM and a terminal's SIGINT both
stop the accept loop, let the requests in flight finish and leave a line saying so. A
streamed turn can run past `TimeoutStopSec`, at which point systemd cuts it.

### In a container

The image is the same binary with the defaults a container wants — `0.0.0.0:3050`, the
caller's key forwarded per request, nothing configured — and the entrypoint is the
binary with no `CMD`, so the argument list is the command line this build already has:

```sh
docker build -t bifrost .

# A configuration, mounted where the command names it.
docker run --rm -v "$PWD/bifrost.toml:/etc/bifrost/bifrost.toml:ro" \
  bifrost --check --config /etc/bifrost/bifrost.toml

# Serving, with the port published and the state directory on a volume.
docker run -d --name bifrost -p 3050:3050 -v bifrost-state:/var/lib/bifrost bifrost
```

`--check`, `--print-config`, `--token-new`, `--token-list` and `--token-revoke` are the
same commands here as they are on a host, and the three token ones read the same
configuration the server reads — which is why they have to be run against the same
volume. A container has no `ExecStartPre` and does not need one: the process validates
the configuration before it binds, so a file this build cannot use fails the container
rather than the first request.

`docker-compose.yml` is the unit file's equivalent, and every hardening line in it is
one of the unit's. The container runs as the same unprivileged account, drops every
capability and may not gain one, and has a read-only filesystem with one named volume —
`/var/lib/bifrost`, where `var/tokens.json` lands when this deployment issues tokens.
`stop_grace_period` is its `TimeoutStopSec`, for the same reason: a streamed turn can
run past the ten seconds a container is given by default. The healthcheck is the image's
own rather than a second copy in the compose file, and it reads `PORT` from the
environment, so moving the port does not leave the probe behind.

### When the key stays in the container

The arrangement `[access]` describes is three lines of configuration in a container as
well, and one of them is fixed by where a key can be mounted. Two commands put a copy
where the process can read it:

```sh
# The process inside runs as uid 10001, so a file at mode 0600 under your own uid is a
# file it cannot read. A copy it owns is what to give it -- not a looser mode on the
# original, which is the credential itself.
sudo install -d -o 10001 -g 10001 -m 0750 /srv/bifrost
sudo install -o 10001 -g 10001 -m 0400 ~/.commandcode/auth.json /srv/bifrost/auth.json
```

```toml
# bifrost.toml, in the checkout
[access]
enabled = true
key_file = "/etc/bifrost/auth.json"   # where the overlay mounts the key
tokens_file = "var/tokens.json"       # relative: /var/lib/bifrost, the state volume
```

```sh
BIFROST_KEY_FILE=/srv/bifrost/auth.json \
  docker compose -f docker-compose.yml -f docker-compose.access.yml up -d

# A token is written into the volume the server reads, so this is the same file it sees.
docker exec bifrost /usr/local/bin/bifrost \
  --config /etc/bifrost/bifrost.toml --token-new laptop --rpm 60 --concurrency 2
```

`docker-compose.access.yml` is an overlay rather than a second deployment: the image,
the hardening, the port and the state volume stay the base file's, and what it adds is
the configuration to read and the key to serve with, both mounted `:ro`. It reads
`BIFROST_KEY_FILE` for the host path — a line of `.env`, which is ignored here, rather
than something to type on every command.

Two things differ from a deployment that forwards its callers' keys. `/status` takes the
token a turn takes, because a row per issued token names callers and that is the one page
here that is not anonymous. And a caller sending `user_…` is refused: a key that still
worked would be one no revocation reaches.

Rotating the account key is a restart, because it is read once at startup. Issuing and
revoking are not: `var/tokens.json` is re-read while this serves, which is what makes
`--token-new` and `--token-revoke` things done to a running deployment.

Images are built for `linux/amd64` and `linux/arm64` by `.github/workflows/docker.yml`
and published to GHCR: a `v*` tag publishes the version it names and moves `latest`, a
manual run from main publishes `edge`, and every image also carries `sha-<commit>` —
the tag to name when reporting something, because it is the only one that cannot move.

```sh
docker run --rm ghcr.io/ctl456/bifrost:latest --help
```

## Troubleshooting

| Symptom | What it is |
|---|---|
| `Missing API key` | no `user_…` token in `Authorization: Bearer` or `x-api-key` |
| `401 MODEL_NOT_IN_PLAN` | the model is real but not in this account's plan; the error comes from the upstream, in the client's own error shape. Pick one from `/v1/models`, or write a rule that points this name at one of them |
| A client hangs, then times out | a reasoning model thinking before its first token. `/v1/messages` sends comment-frame heartbeats for exactly this; a client that cannot tolerate them will still time out |
| A pre-flight is being refused | it never fails a turn, so it leaves no trace in any response — look at the log for the warning, and remember a missing `User-Agent` gets a `403` before the path is even read |
| A drift line in the log | the published client moved ahead of `cc/1.53.1`. The version reported upstream does not move on its own, so re-read the package and re-align the dialect |
| Config will not load | run `--check`; it names the setting. Unknown keys are rejected on purpose |

## Checks

```sh
cargo test                                                 # the whole gate
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
tools/smoke.sh --generate                                  # a build, a running server, one real turn
```

The first three run on every push and pull request, from
`.github/workflows/ci.yml`.

`tools/smoke.sh` starts a build and talks to the live service — health, the counts,
the model catalogue, the access line — then starts a second deployment of the same
build that issues tokens, and reads both logs back: the access line each request left,
the warnings a refused pre-flight shows up in, that no credential reached either log,
and a stop signal answered with a clean exit. Everything it asks of the issuing
deployment it asks with a credential that cannot work, so the section costs nothing —
a refusal is decided before the upstream is reached — except the one turn under
`--generate`. `--self-test` points it at a dead port, so the check can prove it is
able to fail.

The image is built on every pull request and every push to main by
`.github/workflows/docker.yml`, which then starts one, asks it `/health` and runs the
probe the image declares against it. That step is what makes the Dockerfile something
CI checks rather than something only a release exercises; publishing is in the same
file, on a tag.

## What has been verified

Protocol shapes and the fingerprint are diffed against the original JavaScript, not
against a reading of it: `fixtures/` holds the vectors, produced by running regions of
`commandcode-proxy/proxy.mjs` verbatim.

End to end, against the live service:

- the three protocols, streaming and whole, with tools, images, `reasoning_effort`
  and prompt caching;
- the wire actually sent, read back from a recording stand-in upstream: the pre-flight
  pair, the envelope's key order, tool definitions as `input_schema`, images as
  `{"type":"image","image":"data:…","mimeType":…}`, and headers (`user-agent: cli`,
  `x-command-code-version`, `x-project-slug`, `traceparent`);
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
- The upstream is asked for a stream even when the client did not ask for one, so the
  proxy's own accounting is the same for both kinds of turn.
