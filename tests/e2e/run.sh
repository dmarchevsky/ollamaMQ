#!/usr/bin/env bash
# E2E test for model load/unload control (Phase 1).
# Needs: python3 (mock backends), cargo (builds the proxy), free ports 11990/11991/11999.
set -u
cd "$(dirname "$0")/../.."

# Free the test ports if a previous run left stragglers.
# Match by test port only — never kill a production proxy on other ports.
pkill -f "mock_backends.py" 2>/dev/null
pkill -f -- "--port 11999" 2>/dev/null
sleep 0.3
if ss -tln 2>/dev/null | grep -qE ":(11990|11991|11999)\b"; then
  echo "ERROR: test ports still in use:"; ss -tln | grep -E ":(11990|11991|11999)\b"; exit 1
fi

SCRATCH="$(mktemp -d /tmp/ollamamq-e2e.XXXXXX)"
trap 'pkill -f "mock_backends.py" 2>/dev/null; pkill -f -- "--port 11999" 2>/dev/null' EXIT
PASS=0; FAIL=0
check() { # name, expected, actual
  if [ "$2" == "$3" ]; then PASS=$((PASS+1)); echo "PASS: $1";
  else FAIL=$((FAIL+1)); echo "FAIL: $1 (expected [$2], got [$3])"; fi
}

cargo build 2>&1 | tail -1

MQ_E2E_CALLS="$SCRATCH/calls.jsonl" python3 tests/e2e/mock_backends.py > "$SCRATCH/mocks.log" 2>&1 &
MOCK_PID=$!
sleep 1

# Isolate from any appconf.yaml in the repo (its backends/load_keep_alive
# would override the test's expectations). The request log goes to the scratch
# dir so section M can read it back.
cat > "$SCRATCH/appconf.yaml" <<YAML
backends: []
models: []
settings:
  request_log_path: $SCRATCH/reqlog.jsonl
YAML

# One client with body-text capture switched off (section M).
cat > "$SCRATCH/clients.yaml" <<'YAML'
clients:
  - name: redact-me
    redact_log: true
    keys:
      - key: e2e-redact-key
YAML

./target/debug/ollamaMQ --no-tui --port 11999 --model-config "$SCRATCH/appconf.yaml" --backend-urls http://127.0.0.1:11990,http://127.0.0.1:11991 > "$SCRATCH/proxy.log" 2>&1 &
PROXY_PID=$!
sleep 2.5

P=http://127.0.0.1:11999
JH='Content-Type: application/json'
# The admin API answers to a credential. This box has no proxy key and no
# dashboard password, so the only door is the one client key in clients.yaml.
AH='Authorization: Bearer e2e-redact-key'
calls() { python3 -c "
import json,sys
want_server, want_path = sys.argv[1], sys.argv[2]
rows = [json.loads(l) for l in open('$SCRATCH/calls.jsonl')]
hits = [r for r in rows if r['server']==want_server and r['path']==want_path]
print(json.dumps(hits[-1]['body'], sort_keys=True) if hits else 'NONE')
" "$1" "$2"; }

echo "=== A0: /admin/* is closed until you say who you are ==="
CODE=$(curl -s -o "$SCRATCH/anon.json" -w '%{http_code}' $P/admin/models)
check "A0: anonymous admin 403" "403" "$CODE"
check "A0: the answer says the control is closed" "yes" \
  "$(grep -q 'admin control is closed' "$SCRATCH/anon.json" && echo yes || echo no)"
check "A0: the answer says what to configure" "yes" \
  "$(grep -q 'hint' "$SCRATCH/anon.json" && echo yes || echo no)"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer not-a-key' $P/admin/models)
check "A0: an unknown key is not a door" "403" "$CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -H "$AH" $P/admin/models)
check "A0: a client key reads the admin API" "200" "$CODE"

echo "=== A: initial /admin/models ==="
ADMIN=$(curl -s -H "$AH" $P/admin/models)
echo "$ADMIN" | python3 -m json.tool | head -30
check "A: backend0 api type"      "Ollama"  "$(echo "$ADMIN" | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["api"])')"
check "A: backend0 loaded"        '["qwen2.5:7b"]' "$(echo "$ADMIN" | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)[0]["loaded_models"]))')"
check "A: backend1 lmstudio flag" "True"    "$(echo "$ADMIN" | python3 -c 'import json,sys; print(json.load(sys.stdin)[1]["lmstudio"])')"
check "A: backend1 loaded"        '["mock/qwen2-7b-instruct"]' "$(echo "$ADMIN" | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)[1]["loaded_models"]))')"

echo "=== B: load llama3 on backend 0 (Ollama, name resolution) ==="
R=$(curl -s -w '\n%{http_code}' -H "$AH" -H "$JH" -X POST $P/admin/models/load -d '{"backend":0,"model":"llama3"}')
check "B: status 202"  "202" "$(echo "$R" | tail -1)"
check "B: canonical"   "llama3:latest" "$(echo "$R" | head -1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["model"])')"
sleep 0.7
check "B: ollama got generate load body" \
  '{"keep_alive": 86400, "model": "llama3:latest", "stream": false}' \
  "$(calls ollama /api/generate)"

echo "=== D: unload on backend 1 (LM Studio, instance id) ==="
R=$(curl -s -w '\n%{http_code}' -H "$AH" -H "$JH" -X POST $P/admin/models/unload -d '{"backend":1,"model":"mock/qwen2-7b-instruct"}')
check "D: status 202" "202" "$(echo "$R" | tail -1)"
check "D: lmstudio unload body" '{"instance_id": "mock/qwen2-7b-instruct"}' "$(calls lmstudio /api/v1/models/unload)"

echo "=== E: unload qwen2.5 on backend 0 (keep_alive 0 path) ==="
R=$(curl -s -w '\n%{http_code}' -H "$AH" -H "$JH" -X POST $P/admin/models/unload -d '{"backend":0,"model":"qwen2.5"}')
check "E: status 202" "202" "$(echo "$R" | tail -1)"
sleep 0.7
check "E: ollama got unload body" '{"keep_alive": 0, "model": "qwen2.5:7b", "stream": false}' "$(calls ollama /api/generate)"

echo "=== F/G/L: error paths ==="
R=$(curl -s -w '\n%{http_code}' -H "$AH" -H "$JH" -X POST $P/admin/models/load -d '{"backend":0,"model":"no-such-model"}')
check "F: unknown model 404" "404" "$(echo "$R" | tail -1)"
R=$(curl -s -w '\n%{http_code}' -H "$AH" -H "$JH" -X POST $P/admin/models/load -d '{"backend":99,"model":"llama3"}')
check "G: unknown backend 404" "404" "$(echo "$R" | tail -1)"
R=$(curl -s -w '\n%{http_code}' -H "$AH" -H "$JH" -X POST $P/admin/models/load -d '{"backend":0,"model":"   "}')
check "L: empty model 400" "400" "$(echo "$R" | tail -1)"

echo "=== H: backend 'any' selector ==="
R=$(curl -s -w '\n%{http_code}' -H "$AH" -H "$JH" -X POST $P/admin/models/load -d '{"backend":"any","model":"qwen2.5:7b"}')
check "H: status 202" "202" "$(echo "$R" | tail -1)"
check "H: picked backend 0" "http://127.0.0.1:11990" "$(echo "$R" | head -1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["backend"])')"

echo "=== I: busy backend rejected ==="
curl -s -X POST $P/api/chat -H "X-User-ID: e2e" -d '{"model":"qwen2.5:7b","messages":[{"role":"user","content":"hi"}]}' > "$SCRATCH/chat.out" &
CHAT_PID=$!
sleep 2
R=$(curl -s -w '\n%{http_code}' -H "$AH" -H "$JH" -X POST $P/admin/models/load -d '{"backend":0,"model":"llama3"}')
check "I: busy backend 409" "409" "$(echo "$R" | tail -1)"
echo "I: error: $(echo "$R" | head -1)"

echo "=== J/K: duplicate op 409 + visible in /admin/models ==="
R=$(curl -s -w '\n%{http_code}' -H "$AH" -H "$JH" -X POST $P/admin/models/load -d '{"backend":1,"model":"mock/llama-8b"}')
check "J: first load 202" "202" "$(echo "$R" | tail -1)"
sleep 0.3
ADMIN=$(curl -s -H "$AH" $P/admin/models)
check "K: op visible in admin state" "load" "$(echo "$ADMIN" | python3 -c 'import json,sys; print(json.load(sys.stdin)[1]["operation"]["action"])')"
R=$(curl -s -w '\n%{http_code}' -H "$AH" -H "$JH" -X POST $P/admin/models/load -d '{"backend":1,"model":"mock/llama-8b"}')
check "J: duplicate 409" "409" "$(echo "$R" | tail -1)"
sleep 2.5  # let the 2s mock load finish

echo "=== M: a client with capture off keeps the entry, drops the text ==="
curl -s -o /dev/null -X POST $P/api/chat -H "Authorization: Bearer e2e-redact-key" \
  -H "$JH" -d '{"model":"qwen2.5:7b","messages":[{"role":"user","content":"a secret prompt"}]}'
# A line is written once the conversation has been quiet for
# request_log_flush_secs (5 s by default), so give the flush its time.
sleep 7
MRESULT=$(python3 - "$SCRATCH/reqlog.jsonl" <<'PY'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
red = [r for r in rows if r.get("redacted")]
plain = [r for r in rows if not r.get("redacted")]
def texts(line):
    return [t.get("content") for t in line.get("turns", []) if t.get("content")]
def sizes(line):
    return [t.get("req", {}).get("bytes") for t in line.get("turns", [])]
ok = (bool(red)
      # a redacted conversation keeps its shape and loses its text
      and not any(texts(r) for r in red)
      and all(all(sizes(r)) for r in red)
      and not any("a secret prompt" in json.dumps(r) for r in rows)
      # and the ones that allowed capture really did store it
      and any(texts(r) for r in plain))
print("yes" if ok else "no")
PY
)
check "M: redacted lines hold no text, plain ones do" "yes" "$MRESULT"

echo "=== final call log ==="
cat "$SCRATCH/calls.jsonl"

wait $CHAT_PID 2>/dev/null
kill $PROXY_PID $MOCK_PID 2>/dev/null
echo
echo "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
