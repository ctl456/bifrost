# Architecture

## The shape

One crate, `bifrost`, and one binary. The module split is the shape of the job:

| Module | What it holds |
|---|---|
| `core` | the canonical request/response IR and the unified error surface |
| `config` | layered, strictly validated configuration |
| `fingerprint` | the per-key device identity, byte-compatible with the official CLI |
| `wire` | the `WireAdapter` contract and the `cc/1.53.1` dialect |
| `protocol` | the `ProtocolAdapter` contract: OpenAI Chat, Anthropic Messages, OpenAI Responses |
| `edge` | axum HTTP surface, admission control, issued tokens, the streaming path, the catalogue, the drift watch |

```
edge (axum/hyper) ── routing, body limits, admission control, streaming
  │
protocol ── decode a client body into the IR, render IR events as that protocol
  │
core ────── canonical IR ── the only vocabulary the two sides share
  │
wire ────── encode the IR into the upstream's dialect, decode its stream back
  │
Command Code API
```

Adapters never call each other. Every conversion goes through the IR, which keeps
the number of conversion paths linear instead of quadratic: three inbound
protocols and three outbound ones is nine pairs, or three decoders and three
encoders. A behavior fixed once — how reasoning is ordered, say — is fixed
everywhere.

## Why the wire version is pluggable

The upstream dialect changes on its own schedule, so it is an adapter trait rather
than a constant: a new dialect is additive rather than an edit to the main path,
two dialects can coexist so a change can be rolled out per key, and an unknown
version is a startup error rather than a silent guess.

The version this build reports is the one it implements, and it does not move on
its own: reporting a new version while speaking the old dialect is a stronger
signal than reporting an old one, so shape and version stay self-consistent. What
an operator gets instead is `wire.drift_watch`, which reads what the client's
package is currently published as — at startup and once a day, through
`wire.drift_registry` — and says so when the two differ. The comparison is
arithmetic rather than textual, so `1.53.10` orders after `1.53.9` and a
prerelease is reported as not ordering rather than guessed at. Nothing about the
answer reaches a request; a registry that is unreachable costs one line in the log.

## Mechanisms

`mechanisms` is a registry, not a code path: a request passes through the
mechanisms its deployment enabled and nothing else. `fingerprint`, `lifecycle` and
`session` reproduce the original's per-key identity and default to on; the two
that add work are opt-in.

`lifecycle` is the pre-flight, and it is what makes `fingerprint` effective rather
than merely computed. A generate envelope carries the working directory and the
environment, but the device identity goes nowhere except its own endpoint — so
without the pre-flight the fingerprint module is faithful and unused, and the
upstream sees a key with no machine behind it. It is `POST /alpha/fingerprint/record`
and `POST /alpha/lifecycle-events`, sent once per key per window of eight hours plus
up to two of jitter, and awaited by the turn that triggers it so the upstream knows
the device before it generates. The window is claimed before the calls go out, so
requests arriving together send one pair between them; the wait is bounded by
`limits.announce_ms`; and a pre-flight that is refused, fails, or never answers is
logged and the turn is served anyway.

That last property matters when one of these is being debugged: because a failed
announcement never fails a turn, a pre-flight being rejected at the edge leaves no
trace in the response. The upstream requires a `User-Agent` on every request and
answers one without it `403 error code: 1010` before looking at the path, so the
whole announcement is silently absent if that header is ever dropped. It is sent,
and pinned by test.

`prompt_cache` turns a client's `prompt_cache_key` into a cache breakpoint on the
last system section, unless the client marked one itself. `timeout_context_hint`
counts idle timeouts that arrive back to back and words the third one as advice to
shorten the conversation rather than as a slow turn — the message depends on how
many have happened, which the edge tracks and the error does not.

## The model catalogue

`/v1/models` is a read of upstream state rather than a conversion, so it stays out
of the IR: the endpoint asks the dialect for a catalogue request, sends it, and
turns the body into ids. The dialect owns the path and the headers, which is why it
is `provider_models` on the wire adapter rather than a path spelled in the edge.

The list is cached for `models.refresh_ms` and shared by the process, and a fetch
in flight is single-flighted behind a second lock, so a fleet starting at once asks
once. Failure is not a reason to invent an answer: the last catalogue the upstream
gave is served until a new one arrives, and only a deployment that has never had
one falls back to the compiled-in table. The original resets to that table on every
failed refresh, which trades a stale-but-true answer for a fresh guess. A body that
is not a catalogue at all is a failure in the same sense, while an empty one is an
answer: the upstream offered nothing, and second-guessing it would offer models the
upstream does not serve.

What the catalogue is not is a statement about the plan. The upstream lists what it
offers, including models a given plan does not include, and those answer `401
MODEL_NOT_IN_PLAN` when they are asked for. Nothing here filters the list — it
cannot, since the entitlement is not in it — which is half of why the model rules
below are explicit rather than inferred.

## The model rules

A client asks for the model it was written to ask for, and an account is served a
particular set of models. `models.aliases` is where those two are reconciled: a
table of patterns, and the rewrite is a prefix match with the longest pattern
winning. Longest-wins is what lets a deployment write a rule for a family and a rule
for one member of it without the two having to be ordered by hand, and an exact name
needs no special case because it is the longest pattern that can match itself.

Three designs were considered and rejected, all for the same reason — they answer a
question the client did not ask:

- **A default model for unmatched names.** A client that misspells a model would get
  an answer from a different one, and the response's `model` would name something the
  client never asked for. `401 MODEL_NOT_IN_PLAN` is the upstream's true answer.
- **Discovering what the account may use, and choosing for the client.** That is a
  policy about which model is good enough, and it belongs to the person, not to the
  proxy. It would also make the answer depend on the catalogue at the moment of the
  turn, so two identical requests could be answered by two different models.
- **Rewriting at the encoder.** The wire adapter would then send one name while the
  edge echoed another. The rewrite happens once, on the canonical request, before
  anything reads or encodes it, so the name asked for upstream, the name reported in
  the response and the name in the access line are one name — and the name the client
  actually sent is kept beside it as `requested_model`.

The rules are not part of the wire dialect: which of this account's models a client's
vocabulary maps onto is the same question for every dialect. The table is off when it
is empty, and validation refuses a rule that could never name a model — an empty
pattern matches everything, which looks like one line and behaves like a rewrite of
the whole catalogue.

## Issued tokens

A deployment is either forwarding its callers' keys or issuing tokens of its own, and
`edge/access.rs` is the second half of that choice. It is a switch rather than a layer
on top of the first arrangement, because the two are exclusive: a deployment that
issued tokens *and* still honoured a `user_…` key would be handing out revocations
that do not revoke. A key sent to it is refused with a `401` that says which
deployment it is talking to.

Three decisions shape the module:

- **The file holds digests, not tokens.** What `--token-new` writes is the sha256 of
  the token, so the file an operator backs up, diffs and reads is not a credential.
  The price is that a lost token is re-issued rather than looked up, which is the
  right price: a token that can be recovered from a file is a token that can be
  recovered from a backup. Comparison is over digests and every entry is compared
  rather than stopping at the match, so the loop's timing does not say which token was
  presented or where it sits.
- **The credential is read whole rather than scanned for a key.** The pass-through
  path looks for the `user_…` shape inside whatever the client sent, which is what
  lets a key wrapped in extra text still resolve. A token is not a shape to be found
  in a string; it is a value that either matches one this deployment issued or does
  not, and `bfr_` is the prefix that lets an operator tell the two kinds apart.
- **The file is re-read when it changes, not at startup.** One `stat` per request
  buys the half of access control that matters: revocation reaches a running
  deployment. A file that cannot be parsed is a file mid-edit, so the tokens already
  loaded are kept and a warning is written; a file that is *gone* is a deployment with
  no tokens and refuses everything, because the loudest thing an accidental deletion
  can do beats silently serving credentials nobody can take away.

Two places a turn holds travel together in `Slots`, because a streamed answer outlives
the handler that opened it: the deployment's own ceiling and the token's. A token's
place released at return time would let one caller open as many streams as it liked
while holding none of them.

What a token buys is a name. It is what the access line records (`token=<name>`), what
sessions are filed under — the identity a client's conversation is remembered by is
the credential, not the shared key, or one caller's context would land in the middle
of another's — and what `/status` reports. The account's key stays on the machine that
was given it.

The catalogue is the one path that reads the key without a caller having
authenticated: `GET /v1/models` is a question about this deployment's account rather
than about a caller, so it is asked with the key this process holds, and the answer is
cached and shared between callers.

## Streaming and backpressure

The upstream answers with newline-delimited JSON and a client is owed server-sent
events, and between the two sits a decision the original makes implicitly and this
makes explicitly: **the response is not committed until there is something to
commit.** A status can only be chosen once, so if the upstream fails, times out or
produces nothing before any client-visible output exists, the honest answer is a
retryable JSON status — and a proxy that had already sent `200 OK` can only
apologise inside a stream event. The first visible frame is what commits the
response; everything before it is a chance to answer properly.

The upstream is read by a task of its own rather than inside the poll loop of the
response body. The body is only polled while the client is reading it, so a client
that stops reading would otherwise stop the reads and keep the upstream connection —
and the tokens it is generating — alive for as long as it felt like. The task and the
bounded channel between them make the client's slowness the upstream's problem
instead of the gateway's memory, and `limits.client_stall_ms` is where the reader is
cut loose.

Two watchdogs sit on that path. The idle timeout measures the upstream: no byte for
`limits.stream_idle_ms` and the turn is a retryable failure. The heartbeat is the
Anthropic endpoint's alone — its clients measure a first-byte timeout and a reasoning
model can think for minutes before it says anything, so a comment frame is sent when
the stream has been quiet for a while.

## Observability

Everything the edge reports goes through one logger: `telemetry.log_level` filters it
and `telemetry.log_format` chooses one JSON object per line or one plain line per
event. Every request gets a line of its own, including the ones that reached no
endpoint, written by the middleware rather than the handler — the requests an
operator goes looking for are the ones that never arrived: a 404, a preflight, a body
refused before it was read.

The line carries what the turn knew when it answered: the method, the path, the
status, the time it took to begin, and then the protocol, the model, the stream flag,
the session and the fingerprint of the key it was billed to, once it had got that far.
A turn a rule pointed at another model records both — `model` is what answered,
`requested_model` is what the client asked for. The key itself never appears: a log is
read by more people than the key is given to.

`GET /status` answers the same question as counts. It is how an operator tells
"nothing is arriving" from "nothing is working" without reading a log. It is
aggregate, and where it names nobody it needs no credential, so nothing in it quotes
a key, a model or a body, and no decision anywhere is taken from a number in it. A
deployment that issues tokens does name callers there, one row each, and that page
takes the same token a turn does: the port is what reaches further than the machine,
and a list of who is using the account is not part of what it should hand out. There
is no metrics endpoint — the setting that suggested one was removed rather than left
unread, because a switch that turns nothing on is a promise the deployment cannot
keep.

## Determinism

A turn is reproducible from its inputs: the device identity is a pure function of the
key and the configured salt, the wire envelope is assembled from the canonical request
alone, and the order of tools, images and cache breakpoints is the input's. `fixtures/`
records what the original JavaScript produces for a set of inputs and the test suite
diffs against it, so a divergence is caught at the layer that caused it rather than
argued about at the protocol boundary.

The clock and the random source are the two things that are not inputs. Entropy is
shared by every mechanism rather than handed out per mechanism, because a second
source would count from zero again and the counter is part of what keeps two generated
values apart; the tests that need a fixed clock pass one in.
