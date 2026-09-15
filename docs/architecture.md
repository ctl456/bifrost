# Architecture

## Layers

1. **Edge** — accepts HTTP, enforces the body ceiling while streaming, propagates
   backpressure, and shuts down gracefully. Nothing protocol-specific lives here.
2. **Inbound adapters** — decode one public protocol and convert it to the
   canonical IR.
3. **Canonical IR** — `bifrost-core`. The only vocabulary adapters share.
4. **Mechanisms** — optional, config-gated behaviors that observe or amend a
   request. Each one is off unless enabled.
5. **Upstream adapters** — encode the canonical request into the upstream's
   dialect, and decode its stream back into canonical chunks.
6. **Stream engine** — incrementally recognizes reasoning, tool-call markup and
   stop sequences, and emits protocol events.
7. **Outbound adapters** — render canonical events as one public protocol.

## Why a canonical IR

Three inbound protocols and three outbound protocols is nine pairs. Translating
pairwise means nine hand-written converters that drift apart. Routing everything
through one IR means three decoders and three encoders, and a behavior fixed
once (say, how reasoning is ordered) is fixed everywhere.

## Why the wire version is pluggable

The upstream dialect changes on its own schedule. Modeling it as an adapter
trait means:

- a new dialect is additive — a new implementation, not an edit to shared code;
- two dialects can coexist, so a change can be rolled out per key;
- an unknown version is a startup error rather than a silent guess.

The version this build reports is the one it implements, and it does not move
on its own: reporting a new version while speaking the old dialect is a stronger
signal than reporting an old one, so shape and version stay self-consistent. The
knob that would have let a deployment do otherwise was removed rather than left
unread — it described something this proxy never does.

That leaves the question of how an operator learns that the client has moved on.
A dialect names the package it was read from, and `wire.drift_watch` reads what
that package is currently published as, at startup and once a day, through
`wire.drift_registry`. The comparison is arithmetic rather than textual: `1.53.10`
is after `1.53.9`, a version this build is ahead of is not reported as drift, and
a pre-release is reported as not ordering rather than guessed at. Nothing about
the answer reaches a request — the version reported upstream stays the implemented
one, and a registry that is unreachable costs a line in the log.

A drift line is a question, not an answer: `tools/check-dialect-alignment.sh`
settles it by comparing the literals the dialect implements against the published
bundle, and exits non-zero only when one of them is gone or has moved.
`docs/wire-alignment.md` explains why literals rather than bytes, and records the
1.54.0 result.

## Mechanisms

Layer 4 is a registry, not a code path: a request passes through the mechanisms
its deployment enabled and nothing else. Each one is a question about the same
canonical request, so they cannot reorder each other's effects.

| Mechanism | Effect | Default |
|---|---|---|
| `fingerprint` | a deterministic per-key device identity | on |
| `lifecycle` | the pre-flight that announces a key to the upstream | on |
| `session` | a per-key session id with expiry and jitter | on |
| `prompt_cache` | a cache breakpoint on the last system section | off |
| `evidence_archive` | the turn's original bytes, plus a journal line | off |
| `timeout_context_hint` | repeated-idle-timeout advice | off |

The first three reproduce the original proxy and are behavior-preserving in the
sense that the upstream sees what it always saw. The last three add work, so they
are off until an operator asks: one of them writes to disk, and the other two
change what is sent upstream or read back from it.

`lifecycle` and `fingerprint` are two switches on one path. The pre-flight is the
only place a device identity is ever sent: a generate envelope carries the working
directory and the environment, but the fingerprint goes to its own endpoint, where
the upstream is asked to remember a machine. So `fingerprint` gates the record and
`lifecycle` gates the pair, and a deployment with the pre-flight off has no
fingerprint as far as the upstream is concerned — which is why the mechanism was
not honored until the pre-flight existed to carry it.

The pair is sent once per key per window, eight hours plus up to two of jitter,
rather than per request: both are facts about a key, and repeating them on every
turn would be noise. The window is claimed before the calls go out, so requests
arriving together send one pair between them instead of one each. The turn that
triggers it waits, because the point of the record is that the device is known
*before* the generate — but the wait is bounded by `limits.announce_ms`, and a
pre-flight that is refused, fails, or never answers is logged and the turn is
served anyway. An announcement is bookkeeping; it does not get to take traffic
down.

`timeout_context_hint` counts timeouts *in a row*. A single timeout is a slow
turn and is reported as one; three of them mean the conversation no longer fits
the window, which is a statement about the request rather than about the network,
so the client is told what to do about it. A turn that finishes clears the run.

## The model catalogue

`/v1/models` is a read of upstream state rather than a conversion, so it stays
out of the IR: the endpoint asks the dialect for a catalogue request, sends it,
and turns the body into ids. The dialect owns the path and the headers — that is
why it is `provider_models` on the wire adapter rather than a path spelled in the
edge — and it is the same dialect, so a new wire version does not leave the
catalogue pointing at an address that no longer exists.

The list is cached for `models.refresh_ms` and shared by the process, and a
fetch in flight is single-flighted behind a second lock, so a fleet starting at
once asks once. Failure is not a reason to invent an answer: the last catalogue
the upstream gave is served until a new one arrives, and only a deployment that
has never had one falls back to the compiled-in table. The original resets to
that table on every failed refresh, which trades a stale-but-true answer for a
fresh guess. A body that is not a catalogue at all is a failure in the same
sense, while an empty one is an answer: the upstream said this key may use
nothing, and second-guessing it would offer models the account cannot request.

## Evidence preservation

Transformations are lossy by design. The rule is that the original must remain
retrievable and verifiable:

- archived bytes are content-addressed, so a handle cannot drift;
- verification re-hashes the blob before accepting any claim about it;
- a failed verification returns the original rather than a partial result.

`evidence_archive` applies that rule to traffic. Three things are kept, each
under its own kind: the client's request body, the upstream's answer — whole for
a non-streaming turn, as the raw events for a streamed one — and one journal line
that ties them together. The line carries the digests, not the bodies, and a
digest of the key rather than the key, because a journal is read by people and
scripts that should not be handed either.

A streamed turn is recorded by the owner of the stream, and the stream ends in
more ways than one: it can finish, fail, time out, or be dropped because the client
stopped reading. The last case is the one with something to learn from it, so the
capture is written when the stream is dropped as well as when it ends. A turn that
never committed hands the capture back to the handler instead, because only the
handler still knows the status the client is about to be given.

The archive is deliberately unable to fail a request. It is written off the async
thread, a write error is logged and dropped, and capture is capped: an archived
stream that hit the cap is marked `truncated` rather than allowed to grow the
process. An operator who loses their disk loses their evidence, not their
traffic.

The same store grows for the life of the deployment, which is the other half of
content addressing: bytes are stored once and never overwritten, so nothing is
replaced and nothing is freed. Retention is the mechanism that closes that, and it
is the one operation in the crate that destroys evidence, so what it did is
recorded where it can be read back. A pass runs at startup and once a day, removes
what is past `audit.retain_days` and then what is over `audit.max_total_mb`, and
leaves a `retention` line in the journal saying how many blobs and bytes went. The
journal itself is never pruned: a line whose bytes are gone still says what
happened, and a journal with holes in it could not be read as one.

A digest with no bytes behind it is a case the read surface has to answer rather
than trip over, which is why the archive reports three things where two would do:
the bytes are here and hash to the digest, the bytes are here and no longer do, or
there are no bytes. The first two are claims about the disk; the third is the
deployment's own retention, and `bifrost --verify` gives it an exit code of its own
(3, against 1 for a claim that did not hold) so a script can tell them apart. The
journal is what remains readable in that case: its line still says which turn the
bytes belonged to, and only the check against the bytes is gone.

`bifrost --audit` is that check widened from one digest to the whole journal, which is
the shape an operator needs after changing retention or after a disk incident: of every
digest the lines name, how many are intact, how many no longer hash to their name, and
how many are gone. It walks the same two pieces the rest of the read surface uses —
`digests_in` for what the journal names, `held` for what the store has — so its counts
cannot drift from what `--verify` answers about any one of the digests. A name is only
looked up when it is shaped like a digest; a line naming anything else is counted apart
and reported, because a journal this build did not write is a finding rather than a line
to skip. The codes are `--verify`'s, over a whole journal instead of one name.

`bifrost --turn DIGEST` is what makes the archive readable rather than only checkable: it
prints the line as it was stored, and then, for each half of that turn, the verdict
`--verify` would give and the bytes behind it. It writes those bytes to stdout instead of
answering with a `String`, because a body is whatever the client sent — it may not be
UTF-8 at all — and a command that decoded one into text would hand back something that no
longer hashes to the digest it is named by. The same reason puts the line out as it is
stored rather than re-rendered: the reader gets the record, not a summary of it. A digest
can be named by more than one line, since identical bytes are stored once, so every line
that names it is handed over; which turn a digest belonged to is not something the digest
can say.

A session is the other unit a reader has, and unlike a digest it is a conversation rather
than a turn: the client may name its own, and a deployment that is not told one generates
it per key. That one field answers three questions — `--journal --session abc` is the
conversation's lines, `--audit --session abc` is how much of it is still on disk, and
`--turn --session abc` hands over its bytes, oldest first — which is why the selector lives
in the shared filter rather than in any one command. It is an equality and not a window: a
line that names no session is not a line about that conversation, while a line whose
timestamp cannot be read is still a line about *something*, which is why `--since` keeps
the undatable ones and `--session` keeps none of them.

Two rules keep the pass from destroying something the deployment asked to keep. A
blob is kept when a journal line inside the window names it, even when the file is
older — identical bytes are stored once, so the file an old turn created can be the
file yesterday's turn was made of, and pruning by file age alone would take
yesterday's evidence. And the window outranks the ceiling: bytes inside the window
are kept even when that exceeds `audit.max_total_mb`, and the pass logs that the
ceiling could not be met. Meeting it by deleting the window would be quietly
destroying what the operator asked to keep, and choosing which turns mattered is
not this process's decision to make.

## Streaming and backpressure

A streamed turn is read by a task of its own and handed to the response body over a
bounded channel. The reason is that the body is only polled while the client is
reading it: read the upstream from inside that poll loop, and a client that stops
reading silently stops the reads, holding the upstream connection — and the tokens
being generated into it — open for as long as it likes, while whatever has already
arrived piles up in memory. A reader task inverts that. The channel is the whole
budget: four chunks, and past that the reader blocks. The client's slowness is then
the upstream's problem rather than the gateway's memory.

Blocking is also the signal. `limits.client_stall_ms` puts a watchdog on the
reader's wait: once a hand-over has been waiting longer than the deployment allows,
the reader is aborted, and aborting it drops the upstream response, which closes
the connection. Cutting the upstream loose is the point — a client that has left
should not keep costing tokens. Zero disables the watchdog, and the upstream is
held until the client itself goes away.

The channel bounds what the reader can get ahead by, so the decoder has to be
bounded too, or the reader is free to drain the channel into a buffer it cannot
forward. A frame is one line of JSON, so a line that grows past
`MAX_LINE_BYTES` without a terminator fails the turn rather than accumulating: the
unbounded growth the channel prevents at the socket is otherwise reachable through
the decoder, and it is reached exactly when the client has stopped reading.

## Observability

The edge reports through one place. `telemetry.log_level` is a filter rather than
a decoration — a message below it is not rendered at all — and `telemetry.log_format`
chooses between one object per line for a collector and one line per event for a
person. Nothing else in the crate decides for itself whether to speak, which is
what makes the configured level mean something.

Every request gets a line, whether or not it reached an endpoint: a 404 and a CORS
preflight are accounted for exactly as a turn is, because they are the requests an
operator is usually looking for. What the turn knew when it answered rides along —
method, path, status, time to the answer, and then the protocol, the model, the
stream flag and the fingerprint of the key it was billed to, once it had got that
far. The key itself never does; the line is read by more people than the key was
given to. The line is written when the answer begins rather than when it ends, so a
stream that runs for minutes leaves a line about the wait its client felt, and what
the stream cost is the evidence archive's question.

The line says what happened to a request after the fact. What an operator asks
first is not about one request but about all of them: is anything arriving, or is
everything failing? `GET /status` answers that with counters — uptime, turns
answered, and the requests that were refused, split by why (no key, a body too
large, a body that was not this protocol's JSON, the in-flight ceiling, the upstream
not answering, an idle timeout, a client that stopped reading). Nothing in it names
a key, a model or a body, which is what lets it be served without a key, and nothing
in it is read by any code path: the counts are reported, not acted on. They cover
the questions the log answers only by being read a line at a time, not the ones the
log answers better.

There is no metrics endpoint, and the setting that suggested one was removed
rather than left unread: a switch that turns nothing on is a promise the
deployment cannot keep. The same reasoning retired the knob that would have
reported a protocol version this build does not implement. `/status` is not that
setting returning under another name: it has no switch, nothing to configure, and
no decision anywhere is taken from it.

## Determinism

The device fingerprint is derived from the API key, never from the host. It must
survive restarts, redeploys and multiple instances, because a fingerprint that
changes is itself a signal. `bifrost-fingerprint` is a byte-for-byte port of the
original implementation, guarded by fixtures generated from that implementation.

What it derives from is one profile — platform, architecture, OS release and
working directory — built once from `[device]` and read by the fingerprint, the
envelope's environment and working directory, the project slug and the lifecycle
metadata. Only the working directory is configurable, and it is configurable in
one place only, so a deployment cannot describe itself two ways; the slug is
derived from the directory rather than set beside it, which is the relationship a
real client has.
