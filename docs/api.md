# API

[← Back to README](../README.md)

ollamaMQ speaks both the Ollama and the OpenAI-compatible API on one port (default `11435`). Point your clients at it instead of at a backend.

## Endpoints

Identify the caller with a client API key or an `X-User-ID` header (see [Request headers](#request-headers)); requests with neither queue per client IP.

- `GET /health` (Internal health check)
- `GET /` (Backend Status)
- `POST /api/generate`
- `POST /api/chat`
- `POST /api/embed`
- `POST /api/embeddings`
- `GET /api/tags` (answered by the proxy — merged from all Ollama-compatible backends; decision-model backends contribute their model *names*, never a live listing, since they implement no `/api/*` route)
- `POST /api/show`
- `POST /api/create`
- `POST /api/copy`
- `DELETE /api/delete`
- `POST /api/pull`
- `POST /api/push`
- `GET/HEAD/POST /api/blobs/{digest}`
- `GET /api/ps`
- `GET /api/version`
- `POST /v1/chat/completions` (OpenAI Compatible)
- `POST /v1/completions` (OpenAI Compatible)
- `POST /v1/embeddings` (OpenAI Compatible)
- `GET /v1/models` (OpenAI Compatible — answered by the proxy, merged from all backends that can list models, and always answered in OpenAI's `data`/`id` envelope regardless of which backend answered first)
- `GET /v1/models/{model}` (OpenAI Compatible)
- `POST /v1/systemone` (System One decision models — CLM, Laya, Kev; routed to those backends **only**)

### Example

```bash
curl -X POST http://localhost:11435/api/chat \
  -H "X-User-ID: developer-1" \
  -d '{
    "model": "qwen3.5:35b",
    "messages": [{"role": "user", "content": "Explain quantum computing."}],
    "stream": true
  }'
```

**Model listings are aggregated.** `GET /api/tags` and `GET /v1/models` are answered by the proxy from **every** compatible backend at once, not routed to one of them. Each backend's own listing is merged (so per-model metadata like `size`, `digest` and `details` is preserved exactly as that backend reported it) and deduplicated by name/id. Clients therefore see everything the proxy can route to, and the list does not change from one poll to the next depending on which backend happened to answer.

**Metadata reads never queue.** `/`, `/api/tags`, `/api/ps`, `/api/version`, `/api/show`, `/v1/models` and `/v1/models/{model}` don't run inference, so they neither wait for a free backend slot nor occupy one — a chat UI polling `/api/tags` on a timer is answered immediately instead of queueing behind a multi-minute generation (or a model load).

Model aliases and Olla-style paths (`/olla/openai/v1/...`) are covered in [Configuration](configuration.md#model-aliases-and-olla-style-paths); decision-model backends (`/v1/systemone`) in [Configuration](configuration.md#decision-model-backends-system-one).

## Request headers

| Header | Meaning |
| --- | --- |
| `Authorization: Bearer <key>` / `X-API-Key` | a client API key (queues the request under that client) or the proxy key — see [Authentication](#authentication) |
| `X-User-ID` | who a keyless request belongs to; without it the request queues as `ip-<address>` |
| `X-Session-Id` | the conversation id, used by [sticky routing](scheduling.md#sticky-routing-session-affinity) |
| `X-Session-Name` | a readable label for the conversation (`#refactor-auth`) |
| `X-Session-Priority` | `high` or `top` — see [Session labels & priority](scheduling.md#session-labels--priority) |
| `X-Redact-Log` | keep this request's text out of the conversation log (presence is the flag) — see [Request log](request-log.md) |
| `X-Compaction`, `X-Compaction-Of` | mark a context-summarization call and name its conversation — see [Context compaction](scheduling.md#context-compaction) |

Every other header is forwarded to the backend untouched (a client key is stripped first).

## Authentication

By default, `ollamaMQ` only listens on loopback (`127.0.0.1`), so it is only reachable from the local machine. If you expose it to the network with `--host 0.0.0.0`, set the `OLLAMA_MQ_API_KEY` environment variable to a secret key to protect the proxy. Auth is **opt-in**: if the variable is unset or empty, behavior is unchanged and no key is required.

When a key is set, every request **except `/health`** must present it via one of two headers:

- `Authorization: Bearer <key>`
- `X-API-Key: <key>`

The key of any enabled client (see [Clients and API keys](web-ui.md#clients-and-api-keys)) is accepted too. Requests with a missing or wrong key get `401 Unauthorized` (`{"error":"unauthorized"}`). The `/health` endpoint always stays unauthenticated so health checks and Docker healthchecks keep working.

`/admin/*` keeps its own rule and does **not** inherit this one — see [Model Control](#model-control-admin-api). It fails closed: with no proxy key and no dashboard password it answers `403` to everyone, client keys included, until you create a credential. `/health` and `/metrics` are outside both rules.

```bash
# Enable auth
OLLAMA_MQ_API_KEY=supersecret ollamaMQ

# Call the API with the key (either header works)
curl -X POST http://localhost:11435/api/chat \
  -H "Authorization: Bearer supersecret" \
  -H "X-User-ID: developer-1" \
  -d '{"model": "llama3", "messages": [{"role": "user", "content": "Hi"}]}'

# ...or with X-API-Key
curl http://localhost:11435/api/tags -H "X-API-Key: supersecret"

# /health never requires a key
curl http://localhost:11435/health
```

In Docker, just pass the variable through — the binary reads it directly from the environment:

```bash
docker run -d --name ollamamq -p 11435:11435 \
  -e OLLAMA_MQ_API_KEY=supersecret \
  ghcr.io/dmarchevsky/ollamamq
```

## Model Control (Admin API)

Models can be loaded and unloaded on connected backends without touching the backend servers. Supported backends:

- **Ollama** — no dedicated endpoint exists; loading is a `POST /api/generate` with an empty prompt (the scheduler loads the model and returns immediately), unloading is the same call with `keep_alive: 0`.
- **LM Studio 0.3.6+** — the native `POST /api/v1/models/load` / `POST /api/v1/models/unload` endpoints (unload targets a specific loaded instance). Older LM Studio versions are detected and a friendly error points at the `lms load`/`lms unload` CLI.
- Plain OpenAI backends (vLLM, etc.) have no control API and are rejected with a clear error.
- **System One decision-model servers (CLM, Laya, Kev)** have no model-control API either — no load/unload, no `keep_alive`, not even `/api/ps`. Residency is configured on the server itself (e.g. `LAYA_PRELOAD`, `LAYA_MAX_LOADED`), so a `models:` entry for these backends only makes the name routable and pins it; it never starts a load.

All three endpoints are served by the proxy itself (they are never proxied to a backend). They are **not** open by default: an admin call is accepted from

1. the proxy key, when `OLLAMA_MQ_API_KEY` is set (`Authorization: Bearer <key>` or `X-API-Key`),
2. otherwise the dashboard's own credential — a live session cookie, or the same user and password as HTTP basic auth (`curl -u admin:…`). The cookie is set with `Path=/ui`, so a browser will not offer it to `/admin/*` on its own; pass it yourself (`curl -b omq_session=…`). This works whether or not the dashboard is listening on this interface: the credential is what counts, so an operator can keep `settings.web_ui.listen` on `127.0.0.1` and still administer over the network,
3. otherwise — and only when neither of the above exists — any **enabled** client key (see [Clients and API keys](web-ui.md#clients-and-api-keys)).

Anything else gets `403` with a body naming what to configure, and the startup log says which door is open (or that the control is closed). A client key is the last resort rather than the rule: a box full of client keys is not a box whose clients may reconfigure it. The dashboard's own model controls go through `/ui/api/*` and follow the dashboard's login.

### `GET /admin/models`

Per-backend inventory: index, URL, online status, detected API type, LM Studio flag, active request count, available models, currently loaded models, and any in-flight control operation.

```bash
curl -s http://localhost:11435/admin/models | python3 -m json.tool
```

### `POST /admin/models/load` and `POST /admin/models/unload`

Body:

```json
{ "backend": 0, "model": "llama3", "num_ctx": 16384, "keep_alive": 3600, "identifier": "big-ctx" }
```

`backend` accepts:

- a **numeric index** (0-based, in the order of `--backend-urls`),
- a **URL** (exact or substring match), or
- `"any"` — the proxy picks a suitable online, idle backend (for load: the first backend where the model resolves; for unload: the first backend that actually has it loaded).

`model` is resolved against each backend's model list: exact match, then `:latest`/case-insensitive, then a *unique* substring (admin ops only). Ambiguous names are rejected — the proxy never guesses.

Optional fields (load only):

- `num_ctx` — max context window, sent as Ollama `options.num_ctx` or LM Studio `context_length` (omitted = backend default).
- `keep_alive` — overrides `--load-keep-alive` (seconds) for this load. `-1` = keep forever.
- `identifier` — free-form label shown with the operation in the dashboard and API responses.

Responses:

| Status | Meaning |
| ------ | ------- |
| `202`  | Accepted — the operation started in the background (response body contains the canonical model name and the backend URL). |
| `400`  | Bad request — empty model, backend offline, unsupported backend type, or unloading a model that is not loaded. |
| `404`  | Unknown backend, model not found on the backend, or no suitable backend for `"any"`. |
| `409`  | Conflict — another control operation is already in flight on that backend, or the backend is busy with active requests. |

```bash
# Load (llama3 resolves to llama3:latest). ADMIN stands for the credential the
# box accepts — the proxy key, or `-u admin:<password>` for the dashboard's.
ADMIN='Authorization: Bearer supersecret'
curl -s -X POST http://localhost:11435/admin/models/load \
  -H "$ADMIN" -H "Content-Type: application/json" \
  -d '{"backend": 0, "model": "llama3"}'

# Unload by URL (frees GPU memory on Ollama)
curl -s -X POST http://localhost:11435/admin/models/unload \
  -H "$ADMIN" -H "Content-Type: application/json" \
  -d '{"backend": "http://10.0.0.2:11434", "model": "qwen2.5:7b"}'

# Let the proxy pick a backend
curl -s -X POST http://localhost:11435/admin/models/load \
  -H "$ADMIN" -H "Content-Type: application/json" \
  -d '{"backend": "any", "model": "llama3:latest"}'

# The inventory, with the dashboard's credential instead
curl -s -u admin:change-me http://localhost:11435/admin/models | python3 -m json.tool
```

**Note:** Ollama keeps a model loaded for `keep_alive` seconds only (its own default is 5 minutes). A control "load" sends the `--load-keep-alive` value (default 24 h) so the model actually stays resident; an "unload" sends `keep_alive: 0`.
