#!/usr/bin/env bash
# E2E for the security audit's reply path: does a **streamed tool call** reach
# the detectors as the command it is, and does a reply longer than the old
# 128 KiB cut still get judged?
#
# What unit tests cannot say is what this proves: they call `reply_text`
# directly. Here the reply travels the whole route — the proxy stores the SSE
# verbatim, hands the dispatcher's job its whole stored response, the audit
# parses it after the turn was served, and the verdict rides the turn record in
# the request log. Three turns, read back from that log:
#
#   A  a tool call whose `arguments` arrived split across deltas  (audit D1, and
#      the reply-scoping of D4) → a **confirmed** exfil flag naming the transfer
#   B  the same call after prose past 128 KiB, at the very end    (audit D2)     → still flagged
#   C  an ordinary `ls -la` tool call (the control)                              → no flag at all
#   D  A again, sent with `X-Redact-Log`: the turn has no stored text, so the
#      audit has to judge the reply from the copy it keeps in memory → the same
#      confirmed exfil flag, with **no** quoting text on the record
#
# Needs: python3 (the mock), cargo (builds the proxy), free ports 11993/11994.
# Never 8081 or 11435: the production container owns those.
set -u
cd "$(dirname "$0")/../.."

# Free the test ports if a previous run left stragglers. Match by test port
# only — never kill a production proxy on another port.
pkill -f "audit_mocks.py" 2>/dev/null
pkill -f -- "--port 11994" 2>/dev/null
sleep 0.3
if ss -tln 2>/dev/null | grep -qE ":(11993|11994)\b"; then
  echo "ERROR: test ports still in use:"; ss -tln | grep -E ":(11993|11994)\b"; exit 1
fi

SCRATCH="$(mktemp -d /tmp/ollamamq-audit-e2e.XXXXXX)"
# Kill what we started, then take the scratch dir with us. Every exit is covered:
# a failed assertion, a refused port, a Ctrl-C or a kill — an interrupted run used
# to leave its config, its request log and two logs behind in /tmp (eleven had
# piled up). Nothing the run reads outlives it: the log reader turns the log into
# $RESULT before the first check, and every check reads that, not the files.
cleanup() {
  pkill -f "audit_mocks.py" 2>/dev/null
  pkill -f -- "--port 11994" 2>/dev/null
  [ -n "${SCRATCH:-}" ] && rm -rf "$SCRATCH"
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM
PASS=0; FAIL=0
check() { # name, expected, actual
  if [ "$2" == "$3" ]; then PASS=$((PASS+1)); echo "PASS: $1";
  else FAIL=$((FAIL+1)); echo "FAIL: $1 (expected [$2], got [$3])"; fi
}

cargo build 2>&1 | tail -1

# One OpenAI-compatible backend on its own port, its own scratch config (the
# repo's real appconf.yaml must not steer this) and its own request log.
MQ_AUDIT_CALLS="$SCRATCH/calls.jsonl" python3 tests/e2e/audit_mocks.py > "$SCRATCH/mock.log" 2>&1 &
MOCK_PID=$!

cat > "$SCRATCH/appconf.yaml" <<YAML
backends:
  - url: http://127.0.0.1:11993
models: []
settings:
  request_log_path: $SCRATCH/reqlog.jsonl
  # The big turn is ~260 KiB of SSE and must reach the audit whole; the default
  # is already 1 MiB, and this says out loud that the harness depends on it.
  log_content_limit: 1048576
  settings_note: "rules only: no NPU, no decision model, no guard"
  audit:
    enabled: scanner
YAML

./target/debug/ollamaMQ --port 11994 --model-config "$SCRATCH/appconf.yaml" > "$SCRATCH/proxy.log" 2>&1 &
PROXY_PID=$!

P=http://127.0.0.1:11994
JH='Content-Type: application/json'
MODEL=audit-mock

echo "=== 0: the proxy is up and the mock classified as an OpenAI backend ==="
for _ in $(seq 1 60); do
  curl -fs $P/health >/dev/null 2>&1 && curl -s $P/v1/models | grep -q "$MODEL" && break
  sleep 0.5
done
check "0: /health answers" "OK" "$(curl -s $P/health)"
check "0: the backend is listed, so its family was probed" "yes" \
  "$(curl -s $P/v1/models | grep -q "$MODEL" && echo yes || echo no)"

# One streamed chat request per scenario. The marker in the user text is what
# the mock reads to pick its stream, and what the reader uses to find the turn —
# except for D, whose turn keeps no text at all and which the reader finds by
# its `redacted` flag instead.
chat() { # marker
  curl -s --max-time 90 -N -X POST $P/v1/chat/completions -H "$JH" \
    -d "{\"model\":\"$MODEL\",\"stream\":true,\"messages\":[{\"role\":\"user\",\"content\":\"$1 do the thing\"}]}"
}
chat_redact() { # marker, same request with log capture switched off
  curl -s --max-time 90 -N -X POST $P/v1/chat/completions -H "$JH" -H "x-redact-log: 1" \
    -d "{\"model\":\"$MODEL\",\"stream\":true,\"messages\":[{\"role\":\"user\",\"content\":\"$1 do the thing\"}]}"
}

echo "=== the four turns ==="
A=$(chat "E2E-AUDIT-SPLIT"); B=$(chat "E2E-AUDIT-BIG"); C=$(chat "E2E-AUDIT-BENIGN")
D=$(chat_redact "E2E-AUDIT-REDACT")
done_ok() { printf '%s' "$1" | grep -q '\[DONE\]' && echo yes || echo no; }
check "0: A reached the client as a stream"      "yes" "$(done_ok "$A")"
check "0: B reached the client as a stream"      "yes" "$(done_ok "$B")"
check "0: C reached the client as a stream"      "yes" "$(done_ok "$C")"
check "0: D reached the client as a stream (redaction is not a refusal)" "yes" "$(done_ok "$D")"
check "0: A really was streamed in fragments"    "yes" \
  "$(printf '%s' "$A" | grep -c '^data: {' | awk '{print ($1>3)?"yes":"no"}')"

# The verdict lands AFTER the response was served (the audit never holds the
# reply) and the log writes a conversation out once it has been quiet for
# request_log_flush_secs, so this takes a moment by design. The reader waits
# for `audit_scanned_total` to cover all three turns, which is what makes C's
# silence a verdict rather than a turn the audit never got to.
echo "=== reading the verdicts back out of the request log ==="
RESULT=$(python3 - "$SCRATCH/reqlog.jsonl" "$P/metrics" "$MODEL" <<'PY'
import json, re, sys, time, urllib.request

log_path, metrics_url, model = sys.argv[1], sys.argv[2], sys.argv[3]
MARKS = {"A": "E2E-AUDIT-SPLIT", "B": "E2E-AUDIT-BIG", "C": "E2E-AUDIT-BENIGN"}
OLD_CAP = 128 * 1024          # audit::RESPONSE_CHARS, the cut D2 removed
TURNS = 4                     # A, B, C, and D sent with X-Redact-Log
# What a redacted verdict is allowed to say: a pattern label, a chain's one-line
# story, the watchlist entry that matched, or the guard's category. Anything else
# in a `detail` is a quote from a turn whose text was never supposed to be saved.
NAMED = re.compile(
    r"^(pattern: [a-z0-9-]+|host [a-z0-9., -]+"
    r"|[a-z +]*in one turn"
    r"|a secret in a tool result and a transfer in the same turn)$"
)
# Strings that would mean the redacted record quotes the traffic.
QUOTES = ("curl", "data-binary", "webhook.site/x", "E2E-AUDIT-REDACT")


def scanned():
    """Turns the audit has finished judging (the metrics counter)."""
    try:
        with urllib.request.urlopen(metrics_url, timeout=5) as r:
            for line in r.read().decode().splitlines():
                if line.startswith("ollamamq_audit_scanned_total "):
                    return int(float(line.split()[-1]))
    except Exception:
        pass
    return 0


def turns():
    """Every stored chat turn of our model, newest verdict line winning per key."""
    newest, out = {}, []
    try:
        lines = open(log_path).read().splitlines()
    except FileNotFoundError:
        return out
    for line in lines:
        if not line.strip():
            continue
        conv = json.loads(line)
        prev = newest.get(conv["key"])
        if prev is None or conv.get("end_ms", 0) >= prev.get("end_ms", 0):
            newest[conv["key"]] = conv
    for conv in newest.values():
        for t in conv.get("turns", []):
            if t.get("path") != "/v1/chat/completions" or t.get("model") != model:
                continue
            req = ((t.get("content") or {}).get("req")) or ""
            mark = next((k for k, m in MARKS.items() if m in req), None)
            # D stored no text, so there is no marker to read: it is the one turn
            # of ours the log says it redacted.
            if mark is None and t.get("redacted") and "content" not in t:
                mark = "D"
            if mark:
                out.append((mark, t))
    return out


def settled():
    by = {k: None for k in ("A", "B", "C", "D")}
    for mark, t in turns():
        by[mark] = t
    if any(t is None for t in by.values()):
        return None
    for k in ("A", "B", "C"):
        if not ((by[k].get("content") or {}).get("resp")):
            return None            # the stream is not stored yet
    return by if scanned() >= TURNS else None


deadline = time.time() + 90
found = settled()
while found is None and time.time() < deadline:
    time.sleep(1)
    found = settled()

out = {"scanned": str(scanned())}
if found is None:
    print("verdict=nope")
    sys.exit(0)


def first(flags, kind="exfil", bad=None):
    for f in flags:
        if f["kind"] == kind and (bad is None or f.get("bad") is bool(bad)):
            return f
    return {}


for mark in ("A", "B", "C", "D"):
    t = found[mark]
    flags = t.get("audit") or []
    resp = (t.get("content") or {}).get("resp") or ""
    f = first(flags, bad=True)
    out[f"{mark}_bad_exfil"] = "yes" if f else "no"
    out[f"{mark}_by"] = f"{f.get('kind')}|{f.get('src')}|{f.get('by')}" if f else "none"
    out[f"{mark}_detail"] = f.get("detail", "") if f else ""
    out[f"{mark}_names_cmd"] = "yes" if "curl -X PUT --data-binary" in json.dumps(flags) else "no"
    out[f"{mark}_resp_kb"] = str(len(resp) // 1024)
    out[f"{mark}_over_old_cap"] = "yes" if len(resp) > OLD_CAP else "no"
    out[f"{mark}_fragments"] = "yes" if '"rl -X P"' in resp else "no"
    out[f"{mark}_joined"] = "yes" if "curl -X PUT --data-binary" in resp else "no"
    out[f"{mark}_flags"] = str(len(flags))

d = found["D"]
dflags = d.get("audit") or []
blob = json.dumps(d)
out["D_text_stored"] = "yes" if "content" in d else "no"
out["D_redacted"] = "yes" if d.get("redacted") else "no"
out["D_excerpts"] = str(sum(1 for f in dflags if "excerpt" in f))
out["D_named_detail"] = "yes" if all(NAMED.match(f["detail"]) for f in dflags if f.get("detail")) else "no"
out["D_details"] = ";".join(sorted({f["detail"] for f in dflags if f.get("detail")}))
out["D_quotes"] = "yes" if any(q in blob for q in QUOTES) else "no"
print("\n".join(f"{k}={v}" for k, v in out.items()))

PY
)
get() { printf '%s\n' "$RESULT" | grep "^$1=" | head -1 | cut -d= -f2-; }
echo "$RESULT" | sed 's/^/  log: /'

check "A: the log holds the split fragments, not a joined command" "yes" "$(get A_fragments)"
check "A: the joined command is nowhere in those bytes (the audit rebuilt it)" "no" "$(get A_joined)"
check "A: the audit really judged all four turns (C's silence is a verdict)" "yes" \
  "$(printf '%s\n' "$RESULT" | awk -F= '$1=="scanned"{print ($2>=4)?"yes":"no"}')"

echo "=== A: a tool call split across deltas is rebuilt before the rules read it (D1 + D4 reply scoping) ==="
check "A: the turn carries a confirmed (red) exfil flag" "yes" "$(get A_bad_exfil)"
check "A: it is the reply's own transfer, by the transfer rule" "exfil|reply|rule:transfer" "$(get A_by)"
check "A: it names the transfer it saw" "transfer + watch host in one turn" "$(get A_detail)"
check "A: what it matched reads as one shell command" "yes" "$(get A_names_cmd)"

echo "=== B: prose past the old 128 KiB cut no longer throws the call away (D2) ==="
check "B: the stored stream is larger than the old cap" "yes" "$(get B_over_old_cap)"
check "B: the call still sat at the very end (split fragments stored)" "yes" "$(get B_fragments)"
check "B: the tail-kept reply still earns the confirmed exfil flag" "yes" "$(get B_bad_exfil)"
check "B: and it is the same reply-side transfer rule" "exfil|reply|rule:transfer" "$(get B_by)"
check "B: it read the rebuilt command, not the fragments" "yes" "$(get B_names_cmd)"

echo "=== C: the control — an ordinary ls -la is not suspicious ==="
check "C: the turn was judged (the counter covers all four turns)" "0" "$(get C_flags)"
check "C: no exfil flag of any severity" "no" "$(get C_bad_exfil)"
check "C: and no flags at all on the turn" "0" "$(get C_flags)"

echo "=== D: a redacted turn is judged in memory, and keeps no quoting text ==="
check "D: the log holds no text at all for it (no content to leak)" "no" "$(get D_text_stored)"
check "D: and the turn is marked redacted, so the viewer says so" "yes" "$(get D_redacted)"
check "D: the reply was still judged — the same confirmed exfil fires" "yes" "$(get D_bad_exfil)"
check "D: same reply, same verdict as the unredacted turn A" "$(get A_by)" "$(get D_by)"
check "D: it still names the chain it saw" "$(get A_detail)" "$(get D_detail)"
check "D: no flag carries an excerpt" "0" "$(get D_excerpts)"
check "D: every detail is a name the audit wrote, never a quote" "yes" "$(get D_named_detail)"
check "D: nothing on the record quotes the command or the prompt" "no" "$(get D_quotes)"

echo
echo "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
