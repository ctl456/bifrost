#!/usr/bin/env bash
# Check a running build against the live service.
#
# The unit tests prove the shapes against recorded vectors, and no test can prove
# that a request the upstream accepts is the one this build sends. A header the service rejects arrives as a failed request that this proxy
# logs and then ignores — the turn is served anyway — so the failure is invisible
# in the response. That is the gap this closes: it is the check that caught a
# missing `User-Agent`, which every test passed over.
#
# Usage:
#   tools/smoke.sh                 health, the counts, the model catalogue, the
#                                  access line, and a second deployment that issues
#                                  tokens
#   tools/smoke.sh --generate      also spend a few tokens on one real turn
#   tools/smoke.sh --key-file P    where the key is (default ~/.commandcode/auth.json)
#   tools/smoke.sh --port N        port to run on (default 3051)
#   tools/smoke.sh --self-test     point the upstream at a dead port, so the
#                                  catalogue check must fail. A check that only
#                                  ever passes is not evidence.
#
# The key is read from the file the CLI writes at login. It is never echoed and
# never written to the temporary configuration: it goes out as the `Authorization`
# of the requests this script makes, exactly as a client's would. The issuing
# deployment is told where that file is — a path, which is the one thing about the
# key that is safe to write down — because holding the key itself is what it is for.
#
# The last section reads the server's own access lines back: one line per request,
# carrying the model and the key fingerprint of the turns that got that far, and
# never the key itself. A request that leaves no line is as much a finding as one
# that leaves a warning.
#
# Exits non-zero when a check fails, and always prints the server's own warnings —
# a pre-flight that is being refused shows up there and nowhere else.

set -euo pipefail

PORT=${PORT:-3051}
KEY_FILE=${KEY_FILE:-"$HOME/.commandcode/auth.json"}
BIN=${BIN:-"target/debug/bifrost"}
GENERATE=0
UPSTREAM="https://api.commandcode.ai"
BASE="http://127.0.0.1:$PORT"

while [ $# -gt 0 ]; do
    case "$1" in
        --generate) GENERATE=1; shift ;;
        --key-file) KEY_FILE=$2; shift 2 ;;
        --port) PORT=$2; BASE="http://127.0.0.1:$PORT"; shift 2 ;;
        --bin) BIN=$2; shift 2 ;;
        --self-test) UPSTREAM="http://127.0.0.1:9"; GENERATE=0; shift ;;
        -h|--help) sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

[ -f "$KEY_FILE" ] || { echo "no key file at $KEY_FILE; run \`cmd login\` first" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is needed to read the key" >&2; exit 2; }

KEY=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["apiKey"])' "$KEY_FILE")
[ -n "$KEY" ] || { echo "the key file has no apiKey" >&2; exit 2; }

work=$(mktemp -d)
server_log="$work/server.log"
# Nothing to kill once the shutdown section has stopped it, and `kill 0` would mean
# the whole process group.
trap 'for pid in "${server_pid:-}" "${issuing_pid:-}"; do if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi; done; rm -rf "$work"' EXIT

# A configuration of its own, so the check does not depend on one being present.
cat > "$work/bifrost.toml" <<TOML
version = 1
host = "127.0.0.1"
port = $PORT
api_base = "$UPSTREAM"
[wire]
adapter = "cc/1.53.1"
drift_watch = false

TOML

[ -x "$BIN" ] || { echo "$BIN is not built; run \`cargo build --release --bin bifrost\`" >&2; exit 2; }

echo "starting $BIN on $BASE"
# The binary is started as a child of this shell rather than of a subshell, so that
# `$server_pid` is the process itself instead of a shell holding it. A stop signal
# sent to the wrong one leaves the gateway running, and the check about stopping it
# measures the shell.
start_dir=$PWD
cd "$work"
"$start_dir/$BIN" > "$server_log" 2>&1 &
server_pid=$!
cd "$start_dir"

for _ in $(seq 1 50); do
    if curl -fsS -m 2 "$BASE/health" >/dev/null 2>&1; then break; fi
    kill -0 "$server_pid" 2>/dev/null || { echo "the server exited:"; cat "$server_log"; exit 1; }
    sleep 0.2
done

failures=0
pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; failures=$((failures + 1)); }

echo
echo "== health =="
if [ "$(curl -fsS -m 5 "$BASE/health")" = "OK" ]; then
    pass "GET /health answers OK"
else
    fail "GET /health"
fi

echo
echo "== the counts =="
# Read before any turn is asked for, so a non-zero turn count here would mean the
# counter is counting something other than what it is named after.
counts_before="$work/status-before.json"
if curl -fsS -m 5 -o "$counts_before" "$BASE/status"; then
    pass "GET /status answers 200"
else
    fail "GET /status"
fi
# The field list is the contract an operator's dashboard reads, and the counts have
# to be about this process alone: this section exists to pin both. `tokens` is in the
# list because the body always carries it — this deployment issues none, so it has to
# be empty rather than absent, which is what says the rows are about tokens instead of
# about the deployment that happens to be running. `cache` is here before a single
# turn has been served, and its rate has to be null rather than 0.0: a rate over no
# tokens is unmeasured, and a dashboard that branched on zero would read "the cache
# never helps" out of a page that had not yet looked.
if python3 - "$counts_before" <<'PY'
import json, sys
status = json.load(open(sys.argv[1]))
fields = sorted(status)
want = sorted([
    "uptime_ms", "inflight", "max_inflight", "turns", "refused", "unauthenticated",
    "too_large", "malformed", "upstream_failed", "timeouts", "client_stalls", "tokens",
    "cache", "cache_hit_rate",
])
if fields != want:
    print("  note  fields: %s" % ", ".join(fields))
    raise SystemExit(1)
if status["tokens"]:
    print("  note  tokens=%r, and this deployment issues none" % status["tokens"])
    raise SystemExit(1)
if status["turns"] != 0:
    print("  note  turns=%r, and nothing has asked for a turn yet" % status["turns"])
    raise SystemExit(1)
if sorted(status["cache"]) != ["cache_write_tokens", "cached_tokens", "completion_tokens", "prompt_tokens"]:
    print("  note  cache: %r" % status["cache"])
    raise SystemExit(1)
if status["cache_hit_rate"] is not None:
    print("  note  cache_hit_rate=%r before any turn" % status["cache_hit_rate"])
    raise SystemExit(1)
PY
then
    pass "the counts carry the documented fields, no token rows, and no turn yet"
else
    fail "the counts are not what /status documents"
fi

echo
echo "== the model catalogue =="
echo "  note  upstream: $UPSTREAM"
# Without a key the build serves its compiled-in table, so the two counts are the
# closest thing to a signal of whether the upstream was actually reached: they
# come from different places.
builtin=$(curl -fsS -m 15 "$BASE/v1/models" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["data"]))' 2>/dev/null || echo 0)
keyed_body="$work/models.json"
code=$(curl -sS -m 30 -o "$keyed_body" -w '%{http_code}' -H "Authorization: Bearer $KEY" "$BASE/v1/models")
keyed=$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))["data"]))' "$keyed_body" 2>/dev/null || echo 0)

if [ "$code" = "200" ] && [ "$keyed" -gt 0 ]; then
    pass "GET /v1/models answers 200 with $keyed model(s)"
else
    fail "GET /v1/models answered $code with $keyed model(s)"
fi
if [ "$keyed" -gt "$builtin" ]; then
    pass "the keyed list is larger than the built-in table ($keyed > $builtin): the upstream was reached"
else
    # A deployment that can only serve its compiled-in table is not reaching the
    # service, which is the one thing a live check exists to notice.
    fail "the keyed list is no larger than the built-in table ($keyed vs $builtin): the upstream was not reached"
fi
python3 - "$keyed_body" <<'PY' 2>/dev/null || true
import json, sys
ids = [m["id"] for m in json.load(open(sys.argv[1]))["data"]]
print("  note  first three: %s" % ", ".join(ids[:3]))
PY

if [ "$GENERATE" = "1" ]; then
    echo
    echo "== one real turn (spends tokens) =="
    body="$work/chat.json"
    code=$(curl -sS -m 120 -o "$body" -w '%{http_code}' \
        -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
        -d '{"model":"deepseek/deepseek-v4-flash","messages":[{"role":"user","content":"Reply with exactly: OK"}],"max_tokens":32,"stream":false}' \
        "$BASE/v1/chat/completions")
    if [ "$code" = "200" ] && python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); sys.exit(0 if d.get("choices") else 1)' "$body" 2>/dev/null; then
        pass "POST /v1/chat/completions answers 200 with a choice"
        python3 - "$body" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
m = d["choices"][0]["message"]
text = m.get("content") or m.get("reasoning_content") or ""
print("  note  %s | finish=%s | usage=%s" % (repr(text[:60]), d["choices"][0].get("finish_reason"), d.get("usage")))
PY
    else
        fail "POST /v1/chat/completions answered $code"
        head -c 300 "$body" | sed 's/^/        /'
    fi

    # The turn that just worked is one the counts have to agree with, which is what
    # makes them a report rather than a decoration.
    counts_after="$work/status-after.json"
    if curl -fsS -m 5 -o "$counts_after" "$BASE/status" \
        && python3 -c 'import json,sys; s=json.load(open(sys.argv[1])); sys.exit(0 if s["turns"] >= 1 and s["upstream_failed"] == 0 else 1)' "$counts_after"
    then
        pass "the counts agree that one turn was answered"
    else
        fail "the counts do not show the turn that was just served"
    fi

    # The cache reading the client was handed and the cache reading `/status` keeps
    # are the same turn's numbers seen from its two sides, and they are compared as a
    # delta rather than absolutely: the page is cumulative, so a check that read the
    # bare total would pass on a deployment whose counters had never moved. This is
    # what makes the totals a report rather than a decoration — a page that counted
    # something other than what the client was billed for would otherwise look
    # perfectly healthy. `cache_hit_rate` is pinned to a number rather than null,
    # because a turn whose prompt was counted is exactly the case it must stop being
    # absent in.
    if python3 - "$counts_before" "$counts_after" "$body" <<'CHECKEof'
import json, sys

try:
    before, after, body = (json.load(open(path)) for path in sys.argv[1:4])
except Exception as error:
    print("  note  %s" % error)
    raise SystemExit(1)

usage = body.get("usage") or {}
prompt = usage.get("prompt_tokens", 0)
cached = (usage.get("prompt_tokens_details") or {}).get("cached_tokens", 0)
completion = usage.get("completion_tokens", 0)
if prompt <= 0 or completion <= 0:
    print("  note  the turn reported no billable usage: %r" % usage)
    raise SystemExit(1)

was, now = before["cache"], after["cache"]
moved = {key: now[key] - was[key] for key in now}
for key, told in (("prompt_tokens", prompt), ("cached_tokens", cached), ("completion_tokens", completion)):
    if moved[key] != told:
        print("  note  %s: the client was told %d, the counts moved %d" % (key, told, moved[key]))
        raise SystemExit(1)

if now["cache_write_tokens"] < was["cache_write_tokens"]:
    print("  note  cache_write_tokens went backwards: %d then %d" % (was["cache_write_tokens"], now["cache_write_tokens"]))
    raise SystemExit(1)

rate = after["cache_hit_rate"]
if rate is None:
    print("  note  cache_hit_rate is null after a turn whose prompt was counted")
    raise SystemExit(1)
if abs(rate - cached / prompt) > 0.001:
    print("  note  cache_hit_rate=%r, but %d of %d tokens were cached" % (rate, cached, prompt))
    raise SystemExit(1)
if after["tokens"]:
    print("  note  tokens=%r, and this deployment issues none" % after["tokens"])
    raise SystemExit(1)
print("  note  prompt=%d cached=%d completion=%d write=%d rate=%.4f"
      % (prompt, cached, completion, moved["cache_write_tokens"], rate))
CHECKEof
    then
        pass "the cache totals moved exactly as much as the usage the client was handed"
    else
        fail "the cache totals do not match the usage the client was handed"
    fi
fi

echo
echo "== the access line =="
# One line per request, whichever endpoint answered it and however it ended. It is
# where a turn's model, stream flag and key fingerprint are visible, so a deployment
# can be read back after the fact.
line_for() { grep -F "\"path\":\"$1\"" "$server_log" 2>/dev/null | tail -1 || true; }
for path in /health /status /v1/models; do
    if [ -n "$(line_for "$path")" ]; then
        pass "the log has a line for $path"
    else
        fail "the log has no line for $path"
    fi
done
if [ "$GENERATE" = "1" ]; then
    turn_line=$(line_for /v1/chat/completions)
    case "$turn_line" in
        *'"model":"'*'"stream":false'*) pass "the turn's line names its model and its stream flag" ;;
        *) fail "the turn's line is missing its model or stream flag: ${turn_line:-<none>}" ;;
    esac
fi
if grep -Fq "$KEY" "$server_log"; then
    # The line names the key by its fingerprint and never by itself: a log is read
    # by more people than the key was ever given to.
    fail "the key itself appears in the log"
else
    pass "the key never appears in the log"
fi

echo
echo "== the same build, issuing tokens =="
# The other way to run it: this process holds the key and hands each caller a token.
# Nothing here spends anything — a refusal is decided before the upstream is reached —
# except the turn under --generate, and what the section checks is the part no unit
# test can see: issuance, revocation and the closed page against a running process,
# and the file all of it lands in.
ISSUING_PORT=$((PORT + 1))
ISSUING_BASE="http://127.0.0.1:$ISSUING_PORT"
issuing_log="$work/issuing/log"
mkdir -p "$work/issuing/var"
# A working directory of its own, so the state this deployment writes is not the state
# the sections above read back. The token file is named absolutely for that same
# reason: the commands below are run from the repository root and the server runs from
# `$work/issuing`, and a relative path would have them reading two different files —
# which is a mistake this section made before it was written to catch it.
cat > "$work/issuing/bifrost.toml" <<TOML
version = 1
host = "127.0.0.1"
port = $ISSUING_PORT
api_base = "$UPSTREAM"
[wire]
adapter = "cc/1.53.1"
drift_watch = false
[access]
enabled = true
key_file = "$KEY_FILE"
tokens_file = "$work/issuing/var/tokens.json"
TOML

# One attempt at a turn, by whatever credential is handed to it. Only a 200 reaches
# the upstream, so asking with a credential that cannot work costs nothing.
attempt() {
    curl -sS -m 120 -o "$work/issuing/turn.json" -w '%{http_code}' \
        -H "Authorization: Bearer $1" -H "Content-Type: application/json" \
        -d '{"model":"deepseek/deepseek-v4-flash","messages":[{"role":"user","content":"Reply with exactly: OK"}],"max_tokens":32}' \
        "$ISSUING_BASE/v1/chat/completions"
}

# The check a unit runs first, on a deployment with no tokens: it has to say so rather
# than pass and leave every request to fail as a 401.
if check=$("$start_dir/$BIN" --check --config "$work/issuing/bifrost.toml" 2>&1) &&
    printf '%s\n' "$check" | grep -q "no token issued yet"; then
    pass "--check reports a deployment that has issued no token yet"
else
    fail "--check on an issuing deployment said: ${check:-<no output>}"
fi

cd "$work/issuing"
"$start_dir/$BIN" --config "$work/issuing/bifrost.toml" > "$issuing_log" 2>&1 &
issuing_pid=$!
cd "$start_dir"
for _ in $(seq 1 50); do
    if curl -fsS -m 2 "$ISSUING_BASE/health" >/dev/null 2>&1; then break; fi
    kill -0 "$issuing_pid" 2>/dev/null || { echo "the issuing deployment exited:"; cat "$issuing_log"; exit 1; }
    sleep 0.2
done

issued=$("$start_dir/$BIN" --token-new laptop --rpm 6 --concurrency 1 --config "$work/issuing/bifrost.toml")
token=$(printf '%s\n' "$issued" | sed -n 's/.*\(bfr_[0-9a-f]\{64\}\).*/\1/p')
if [ -n "$token" ]; then
    pass "--token-new prints a token once"
else
    fail "--token-new printed no token: $issued"
fi

# What is on disk is a hash and a name, at a mode nobody else can read: the file is
# the thing that gets copied around, and a copy of it must not be a credential.
if [ ! -f "$work/issuing/var/tokens.json" ]; then
    fail "no token file was written where the configuration names one"
elif grep -q "$token" "$work/issuing/var/tokens.json"; then
    fail "the token is stored in the clear"
else
    pass "the token file holds no token, only its hash"
fi
mode=$(stat -c '%a' "$work/issuing/var/tokens.json" 2>/dev/null || echo none)
if [ "$mode" = "600" ]; then
    pass "the token file is written 0600"
else
    fail "the token file is mode $mode"
fi

# The page names callers, so it takes one. This is the check that would have caught a
# status page listing who is using the account to whoever reached the port.
anon_code=$(curl -sS -m 5 -o "$work/issuing/anon.json" -w '%{http_code}' "$ISSUING_BASE/status")
if [ "$anon_code" = "401" ] && ! grep -q "laptop" "$work/issuing/anon.json"; then
    pass "the page that names callers answers 401 without one, and names nobody"
else
    fail "GET /status without a credential answered $anon_code: $(head -c 120 "$work/issuing/anon.json")"
fi
page_code=$(curl -sS -m 5 -o "$work/issuing/status.json" -w '%{http_code}' -H "Authorization: Bearer $token" "$ISSUING_BASE/status")
if [ "$page_code" = "200" ] && grep -q '"name":"laptop"' "$work/issuing/status.json"; then
    pass "the same page answers the token, with a row named for it"
else
    fail "GET /status with the token answered $page_code: $(head -c 120 "$work/issuing/status.json")"
fi

# A key is not a token here: a credential that still worked would be one revocation
# does not reach. Neither of these reaches the upstream, so neither costs anything.
if [ "$(attempt "$KEY")" = "401" ]; then
    pass "a key sent to an issuing deployment is refused"
else
    fail "a key sent to an issuing deployment was not refused with 401"
fi
invented="bfr_$(printf '0%.0s' $(seq 1 64))"
if [ "$(attempt "$invented")" = "401" ]; then
    pass "a token that was never issued is refused the same way"
else
    fail "an unissued token was not refused with 401"
fi

# Issuance and revocation reach the process that is running, which is the whole
# reason the file is re-read: a token that only stops working after a restart is a
# token that works until somebody remembers.
phone=$("$start_dir/$BIN" --token-new phone --config "$work/issuing/bifrost.toml" |
    sed -n 's/.*\(bfr_[0-9a-f]\{64\}\).*/\1/p')
phone_code=$(curl -sS -m 5 -o "$work/issuing/status.json" -w '%{http_code}' -H "Authorization: Bearer $phone" "$ISSUING_BASE/status")
if [ -n "$phone" ] && [ "$phone_code" = "200" ] && grep -q '"name":"phone"' "$work/issuing/status.json"; then
    pass "a token issued while the process runs is accepted without a restart"
else
    fail "a token issued while it ran answered $phone_code"
fi
"$start_dir/$BIN" --token-revoke laptop --config "$work/issuing/bifrost.toml" >/dev/null
if [ "$(attempt "$token")" = "401" ]; then
    pass "revocation reaches the next request, not the next restart"
else
    fail "a revoked token was still served"
fi

if [ "$GENERATE" = "1" ]; then
    # The one request here that costs anything, and the only one that proves the
    # token was spent on the account's behalf rather than rejected for its shape.
    if [ "$(attempt "$phone")" = "200" ] &&
        python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); sys.exit(0 if d.get("choices") else 1)' "$work/issuing/turn.json" 2>/dev/null; then
        pass "a token serves a real turn"
        if grep -q '"token":"phone"' "$issuing_log"; then
            pass "the access line names the token, and the key only by fingerprint"
        else
            fail "the access line does not name the token that spent it"
        fi
    else
        fail "a turn with an issued token answered $(head -c 200 "$work/issuing/turn.json")"
    fi
fi

if grep -Fq "$token" "$issuing_log" || grep -Fq "$phone" "$issuing_log"; then
    fail "a token appears in the log"
else
    pass "no token appears in the log"
fi

kill -TERM "$issuing_pid" 2>/dev/null || true
issuing_stopped=0
wait "$issuing_pid" || issuing_stopped=$?
issuing_pid=""
if [ "$issuing_stopped" -eq 0 ] && grep -q "shutting down" "$issuing_log"; then
    pass "the issuing deployment is stopped by SIGTERM and says so"
else
    fail "the issuing deployment left exit status $issuing_stopped"
fi

echo
echo "== shutdown =="
# A service manager sends SIGTERM and a terminal sends SIGINT, and the process
# answers either the same way. That is what lets the unit file use the signal
# systemd sends by default instead of being told to send the one this build happens
# to catch, so it is worth asking the running process rather than the source.
kill -TERM "$server_pid" 2>/dev/null || true
stopped=0
wait "$server_pid" || stopped=$?
server_pid=""
if [ "$stopped" -eq 0 ]; then
    pass "SIGTERM is answered with a clean exit"
else
    fail "SIGTERM left exit status $stopped"
fi
if grep -q "shutting down" "$server_log"; then
    pass "the process said it was shutting down"
else
    fail "the process was stopped without saying so"
fi


echo
echo "== the server's own log =="
if [ -s "$server_log" ]; then
    sed 's/^/  /' "$server_log"
else
    echo "  (empty)"
fi
if grep -q '"level":"warn"' "$server_log" 2>/dev/null; then
    echo
    echo "  WARNING  the server logged a warning. A refused pre-flight appears here"
    echo "           and nowhere else: it never fails the turn it belonged to."
    failures=$((failures + 1))
fi

echo
if [ "$failures" -eq 0 ]; then
    echo "all checks passed"
else
    echo "$failures check(s) failed"
fi
exit $((failures > 0))
