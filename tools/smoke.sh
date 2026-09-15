#!/usr/bin/env bash
# Check a running build against the live service.
#
# The unit tests prove the shapes and the alignment script proves the vocabulary,
# and neither can prove that a request the upstream accepts is the one this build
# sends. A header the service rejects arrives as a failed request that this proxy
# logs and then ignores — the turn is served anyway — so the failure is invisible
# in the response. That is the gap this closes: it is the check that caught a
# missing `User-Agent`, which every test passed over.
#
# Usage:
#   tools/smoke.sh                 health, the counts, the model catalogue, the
#                                  archive's retention pass and reading it back
#   tools/smoke.sh --generate      also spend a few tokens on one real turn
#   tools/smoke.sh --key-file P    where the key is (default ~/.commandcode/auth.json)
#   tools/smoke.sh --port N        port to run on (default 3051)
#   tools/smoke.sh --self-test     point the upstream at a dead port, so the
#                                  catalogue check must fail. A check that only
#                                  ever passes is not evidence.
#
# The key is read from the file the CLI writes at login. It is never echoed and
# never written to the temporary configuration: it goes out as the `Authorization`
# of the requests this script makes, exactly as a client's would.
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
trap 'if [ -n "${server_pid:-}" ]; then kill "$server_pid" 2>/dev/null || true; fi; rm -rf "$work"' EXIT

# A configuration of its own, so the check does not depend on one being present.
cat > "$work/bifrost.toml" <<TOML
version = 1
host = "127.0.0.1"
port = $PORT
api_base = "$UPSTREAM"
[wire]
adapter = "cc/1.53.1"
drift_watch = false

# Turned on so that retention has something to apply to, and so that a deployment
# with the archive on is the one this checks: the mechanism is off by default, and a
# configuration that is only ever exercised with it off is a configuration whose
# wiring nobody has run.
[mechanisms]
evidence_archive = true

[audit]
retain_days = 1
TOML

[ -x "$BIN" ] || { echo "$BIN is not built; run \`cargo build -p bifrost-edge --bin bifrost\`" >&2; exit 2; }

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
# to be about this process alone: this section exists to pin both.
if python3 - "$counts_before" <<'PY'
import json, sys
status = json.load(open(sys.argv[1]))
fields = sorted(status)
want = sorted([
    "uptime_ms", "inflight", "max_inflight", "turns", "refused", "unauthenticated",
    "too_large", "malformed", "upstream_failed", "timeouts", "client_stalls",
])
if fields != want:
    print("  note  fields: %s" % ", ".join(fields))
    raise SystemExit(1)
if status["turns"] != 0:
    print("  note  turns=%r, and nothing has asked for a turn yet" % status["turns"])
    raise SystemExit(1)
PY
then
    pass "the counts carry the documented fields and no turn yet"
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
fi

echo
echo "== the archive and its retention =="
# The pass runs at startup, before the first request is served, so its line is in the
# log whether or not a turn was made. A journal line and a log line are both checked:
# the pass records itself where the deletions can be read back, and says what it did
# where an operator is already looking.
if grep -q "pruned the archive" "$server_log"; then
    pass "the startup pass ran and said what it found"
else
    fail "the startup pass left no line"
fi
if [ -f "$work/var/journal" ] && grep -q '"kind":"retention"' "$work/var/journal"; then
    pass "the pass recorded itself in the journal"
else
    fail "the journal has no retention line"
fi
if [ "$GENERATE" = "1" ]; then
    # A turn that ran with the archive on is a turn whose bytes are on disk and whose
    # line names them, which is the mechanism the retention pass exists to bound.
    blobs=$(find "$work/var/archive" -type f 2>/dev/null | wc -l)
    if [ "$blobs" -ge 1 ] && grep -q '"kind":"turn"' "$work/var/journal"; then
        pass "the turn was archived ($blobs blob(s)) and journalled"
    else
        fail "the turn left $blobs blob(s) in the archive"
    fi
fi

echo
echo "== the access line =="
# One line per request, whichever endpoint answered it and however it ended. This is
# the only place a turn's model, stream flag and key fingerprint are visible without
# turning the evidence archive on, so a deployment can be read back after the fact.
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

if [ "$GENERATE" = "1" ]; then
    echo
    echo "== reading the archive back =="
    # The two commands an operator has instead of a directory layout, run after the
    # stop so that nothing is writing to what they read.
    read_back() { ( cd "$work" && "$start_dir/$BIN" "$@" --config "$work/bifrost.toml" ); }

    if read_back --journal | grep -q '"kind":"turn"'; then
        pass "--journal prints the turn the deployment answered"
    else
        fail "--journal printed no turn"
    fi
    if read_back --journal --kind retention | grep -q '"kind":"retention"'; then
        pass "--journal --kind narrows the answer to one kind"
    else
        fail "--journal --kind retention printed nothing"
    fi

    # The digest is taken from the journal rather than computed here: what is being
    # checked is that the two commands agree about the same deployment, which is the
    # one thing a test with its own fixtures cannot show.
    digest=$(read_back --journal | sed -n 's/.*"sha256":"\([0-9a-f]\{64\}\)".*/\1/p' | head -1)
    if [ -n "$digest" ] && read_back --verify "$digest" | grep -q '^intact'; then
        pass "--verify finds the bytes behind the digest the journal names"
    else
        fail "--verify did not find the bytes of ${digest:-<no digest in the journal>}"
    fi

    # A digest nothing was stored under is the answer retention leaves behind, and it
    # has an exit code of its own rather than being reported as a failed claim.
    absent_reference=$(printf 'f%.0s' $(seq 1 64))
    if output=$(read_back --verify "$absent_reference"); then
        fail "--verify answered 0 for a digest nothing is stored under"
    else
        status=$?
        case "$status:$output" in
            3:*gone*) pass "--verify answers 3/gone for a digest nothing is stored under" ;;
            *) fail "--verify answered $status for an absent digest: $output" ;;
        esac
    fi

    # The audit is the same question asked of every digest the journal names at once,
    # and it is the one an operator runs after changing retention. This deployment
    # prunes nothing, so a run that finds anything less than intact is a real finding
    # rather than the retention it was told to use.
    if audit=$(read_back --audit) &&
        printf '%s\n' "$audit" | grep -q '^entries ' &&
        printf '%s\n' "$audit" | grep -q 'intact [1-9]' &&
        printf '%s\n' "$audit" | grep -q 'tampered 0, gone 0'; then
        pass "--audit counts the whole journal and finds the turn's bytes intact"
    else
        fail "--audit did not find the archive intact: ${audit:-<no output>}"
    fi

    # Handing the turn over is the other half of keeping it: the line as it is stored, and
    # then the bytes of both halves. The digest comes from the journal rather than from
    # here for the same reason as above - what is being checked is that the commands agree
    # about one deployment, and a check with its own fixture cannot show that.
    handed=$(read_back --turn "$digest") || true
    halves=$(printf '%s\n' "$handed" | grep -c '^intact' || true)
    if printf '%s\n' "$handed" | grep -q '^{"timestamp"' &&
        printf '%s\n' "$handed" | grep -q 'in request:' &&
        [ "${halves:-0}" -ge 2 ]; then
        pass "--turn hands over the turn's line and the bytes of both halves"
    else
        fail "--turn did not hand over both halves: ${handed:-<no output>}"
    fi

    # A session is the conversation a client was having, and the same selector reaches
    # the lines and the bytes: two questions asked of one field.
    session=$(read_back --journal | sed -n 's/.*"session":"\([^"]*\)".*/\1/p' | head -1)
    if [ -n "$session" ] && read_back --journal --session "$session" | grep -q '"kind":"turn"'; then
        pass "--journal --session narrows the answer to one conversation"
    else
        fail "--journal --session ${session:-<no session in the journal>} printed no turn"
    fi
    if [ -n "$session" ] && conversation=$(read_back --turn --session "$session") &&
        printf '%s\n' "$conversation" | grep -q 'in request:'; then
        pass "--turn --session hands over the conversation's turns"
    else
        fail "--turn --session did not hand over the conversation: ${conversation:-<no output>}"
    fi
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
