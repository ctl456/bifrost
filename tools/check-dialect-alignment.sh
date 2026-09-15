#!/usr/bin/env bash
# Check the `cc` dialect against the published client it was read from.
#
# The client is published on its own schedule, and `wire.drift_watch` says when it
# has moved. This is what to run when it does: it answers "did anything this
# dialect implements change?" by comparing the literals the dialect depends on.
#
# Why literals and not bytes: the bundle is minified, identifiers are renamed on
# every release, and esbuild annotates every function with its source name for
# stack traces. A byte diff of 1.53.1 against 1.54.0 is 304 added and 260 removed
# string literals, all of them renames or an unrelated feature. What carries the
# contract is the vocabulary itself: paths, header names, payload keys, the event
# name. Those are compared here.
#
# Two forms, because the client writes the two differently:
#   quoted  - `"/alpha/generate"`, `"x-cmd-zdr"`, `"cli_session_exists"`. These are
#             literals in a table or a call, so their use count is stable and a
#             change in the count means the shape moved.
#   bare    - `{workingDir:s,...,recentCommits:[]}`. Object keys are unquoted after
#             minification, and short ones (`mode`, `sessionId`) collapse with
#             unrelated code, so only presence is checked.
#
# Usage: tools/check-dialect-alignment.sh [version]   (default: the latest tag)
#
# Exits non-zero when something the dialect implements is no longer in the
# published client, which means: re-read the package, and if the shape really
# moved, add a new dialect rather than editing this one.
#
# CC_SELFTEST=1 appends a literal that cannot exist, so the run must fail. That is
# the proof this check is capable of failing rather than always agreeing.

set -euo pipefail

WORK=${WORK:-"${TMPDIR:-/tmp}/cc-dialect-alignment"}
# The version this build's dialect was read from, mirroring RELEASE_PACKAGE /
# PROTOCOL_VERSION in bifrost-wire/src/cc/v1531.rs.
READ_FROM=1.53.1

V=${1:-$(curl -sS https://registry.npmjs.org/command-code/latest |
          python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])')}

mkdir -p "$WORK"
cd "$WORK"
for v in "$V" "$READ_FROM"; do
    [ -f "cli-$v.mjs" ] && continue
    echo "fetching command-code@$v"
    curl -sS "https://registry.npmjs.org/command-code/-/command-code-$v.tgz" -o "cc-$v.tgz"
    rm -rf "cc-$v" && mkdir -p "cc-$v"
    tar -xzf "cc-$v.tgz" -C "cc-$v"
    cp "cc-$v/package/dist/cli.mjs" "cli-$v.mjs"
done

CC_READ_FROM="$READ_FROM" python3 - "$V" <<'PY'
import io, os, re, sys

published = sys.argv[1]
read_from = os.environ["CC_READ_FROM"]
selftest = os.environ.get("CC_SELFTEST") == "1"

# Literals this dialect implements, mirroring bifrost-wire/src/cc/v1531.rs.
# `quoted` entries are count-checked, `bare` entries presence-checked.
PATHS = [
    ("quoted", "/alpha/generate"),
    ("quoted", "/alpha/fingerprint/record"),
    ("quoted", "/alpha/lifecycle-events"),
]
HEADERS = [
    ("quoted", "x-command-code-version"),
    ("quoted", "x-cli-environment"),
    ("quoted", "x-project-slug"),
    ("quoted", "x-taste-learning"),
    ("quoted", "x-session-id"),
    ("quoted", "x-cmd-zdr"),
    ("bare", "traceparent"),
]
LIFECYCLE = [
    ("quoted", "cli_session_exists"),
    ("bare", "eventType"),
    ("bare", "sessionId"),
    ("bare", "cliVersion"),
    ("bare", "sess_"),
]
# The keys of the outgoing body, which the client writes as object literals.
CONFIG = [
    ("bare", "workingDir"),
    ("bare", "environment"),
    ("bare", "isGitRepo"),
    ("bare", "currentBranch"),
    ("bare", "mainBranch"),
    ("bare", "gitStatus"),
    ("bare", "recentCommits"),
    ("bare", "structure"),
]
ENVELOPE = [
    ("bare", "permissionMode"),
    ("bare", "threadId"),
    ("bare", "params"),
    ("bare", "promptCache"),
]
# Exactly the params keys the client puts on the wire. `tool_choice` and
# `parallel_tool_calls` are absent here on purpose: the proxied OpenAI surface
# adds those, and the client never sends them.
PARAMS = [
    ("bare", "model"),
    ("bare", "messages"),
    ("bare", "tools"),
    ("bare", "system"),
    ("bare", "max_tokens"),
    ("bare", "stream"),
    ("bare", "temperature"),
    ("bare", "reasoning_effort"),
]
TOOLS = [
    ("quoted", "bash_output"),
    ("quoted", "task_output"),
    ("quoted", "tool_search"),
    ("quoted", "read_multiple_files"),
    ("quoted", "shell_output"),
    ("quoted", "search_tools"),
    ("quoted", "read_file"),
]
ERRORS = [
    ("quoted", "premium_credits_exhausted"),
    ("quoted", "model_not_in_plan"),
]

GROUPS = [
    ("path", PATHS),
    ("header", HEADERS),
    ("lifecycle", LIFECYCLE),
    ("config key", CONFIG),
    ("envelope key", ENVELOPE),
    ("param key", PARAMS),
    ("tool alias", TOOLS),
    ("error code", ERRORS),
]

# Implemented here but never sent by the client, so the published package cannot
# confirm them. Listed so the reason is on the record rather than silently absent.
OURS = [
    ("/provider/v1/models", "GET, from commandcode-proxy; the client never calls it"),
    ("tool_choice", "params key added by the proxied OpenAI surface"),
    ("parallel_tool_calls", "params key added by the proxied OpenAI surface"),
    ("x-cmd-provider-deepseek-internal", "header in the client's table; this proxy does not send it"),
]

# Windows whose surrounding code does not move with refactors, compared as
# ordered literal lists because order is part of the contract there. Only quoted
# literals are visible to this comparison; the unquoted keys above are what catch
# a rename among them, which is why both sections exist.
ANCHORS = [
    ("the generate envelope", "memory:null,taste:null,skills:null"),
    ("the project context", "isGitRepo:!1"),
    ("the header table", '"x-oauth-token"'),
    ("the fingerprint record", '"/alpha/fingerprint/record"'),
    ("the lifecycle payload", '"cli_session_exists"'),
]
WINDOW = 2500

if selftest:
    PATHS.append(("quoted", "/alpha/this-endpoint-does-not-exist"))
    GROUPS[0] = ("path", PATHS)


def load(version):
    return io.open("cli-%s.mjs" % version, encoding="utf-8", errors="replace").read()


def strip_debug_names(text):
    # `__name(fn,"name")` -> `(fn)`: the annotation is a stack-trace label, not a
    # literal, and it moves with every refactor.
    out, i, needle = [], 0, "__name("
    while True:
        j = text.find(needle, i)
        if j < 0:
            out.append(text[i:])
            return "".join(out)
        out.append(text[i:j])
        start, depth, k, quote, comma = j + len(needle), 1, j + len(needle), None, None
        while k < len(text) and depth > 0:
            c = text[k]
            if quote:
                if c == "\\":
                    k += 2
                    continue
                if c == quote:
                    quote = None
            elif c in "\"'`":
                quote = c
            elif c in "([{":
                depth += 1
            elif c in ")]}":
                depth -= 1
                if depth == 0:
                    break
            elif c == "," and depth == 1:
                comma = k
            k += 1
        tail = text[comma + 1:k] if comma is not None else ""
        if comma is not None and re.fullmatch(r'\s*"[^"\\\n]*"\s*', tail):
            out.append("(" + text[start:comma] + ")")
            i = k + 1
        else:
            out.append(needle)
            i = start


LITERAL = re.compile(r'"((?:[^"\\\n]|\\.){1,120})"')
WORD = re.compile(r"^[A-Za-z0-9_./:+ -]{1,120}$")


def literals(text):
    return [s for s in LITERAL.findall(text) if WORD.match(s)]


before = strip_debug_names(load(read_from))
after = strip_debug_names(load(published))

bad = 0
checked = 0

print("== what this dialect implements, as the client writes it ==")
for group, values in GROUPS:
    for form, value in values:
        checked += 1
        needle = '"%s"' % value if form == "quoted" else value
        was, now = before.count(needle), after.count(needle)
        if now == 0:
            print("  GONE     %-12s %-28s (%s)" % (group, value, form))
            bad += 1
        elif form == "quoted" and was != now:
            print("  MOVED    %-12s %-28s %d -> %d uses" % (group, value, was, now))
            bad += 1
        elif form == "quoted":
            print("  ok       %-12s %-28s %d use(s), unchanged" % (group, value, now))
        else:
            print("  ok       %-12s %-28s present, %d use(s) (presence only)" % (group, value, now))

print()
print("== implemented here, not sent by the client (unverifiable against it) ==")
for value, why in OURS:
    print("  n/a      %-24s %s" % (value, why))

print()
print("== the shape around each anchor ==")
for label, anchor in ANCHORS:
    at_a, at_b = before.find(anchor), after.find(anchor)
    if at_a < 0 or at_b < 0:
        print("  GONE     %s" % label)
        bad += 1
        continue
    left = literals(before[max(0, at_a - WINDOW): at_a + WINDOW])
    right = literals(after[max(0, at_b - WINDOW): at_b + WINDOW])
    if left == right:
        print("  same     %-24s %d literals, same order" % (label, len(left)))
    else:
        print("  CHANGED  %-24s missing=%s added=%s" % (
            label, sorted(set(left) - set(right))[:4], sorted(set(right) - set(left))[:4]))
        bad += 1

print()
if bad:
    print("%s disagrees with %s in %d place(s): re-read the package." % (published, read_from, bad))
else:
    print("%s still matches %s: %d literals checked, nothing this dialect implements has changed."
          % (published, read_from, checked))
sys.exit(1 if bad else 0)
PY
