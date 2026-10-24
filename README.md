# ollamaMQ

`ollamaMQ` is a high-performance, asynchronous message queue dispatcher and load balancer designed to sit in front of one or more [Ollama](https://ollama.ai/) or [LM Studio](https://lmstudio.ai/) API instances. It acts as a smart proxy that queues incoming requests from multiple users and dispatches them in parallel to multiple backends using a fair-share round-robin scheduler with least-connections load balancing.

![Rust](https://img.shields.io/badge/rust-2024-orange.svg)
![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Ollama](https://img.shields.io/badge/Ollama-Proxy-7ed321.svg)

## 🚀 Features

- **Multi-Backend Load Balancing**: Distribute requests across multiple Ollama or LM Studio instances using a **Least Connections + Round Robin** strategy. Automatically detects backend API type (Ollama `/api/*` vs OpenAI `/v1/*`) and routes each request only to a compatible backend — even when a specific model is requested, so an Ollama-family call never reaches an LM Studio server that merely lists the same model.
- **Model-Aware Routing**: Automatically identifies the requested model from the request body and routes it only to backends that list that model. Among those, selection is load-aware: first to backends where the model is already loaded and ready (Ollama `/api/ps`, LM Studio loaded instances; vLLM-style backends that don't report loaded state count as ready when they list the model), then to cold backends with nothing loaded, and only as a last resort to a backend that would have to evict a different loaded model — so a request for `qwen3.8-27b` goes to the server that has it in GPU memory instead of making a busy server with another model load it. Name matching is deterministic and bounded — exact, `:tag`/case-insensitive, or publisher-/quant-suffixed variants of the same id (`qwen3.8-27b` reaches `unsloth/qwen3.8-27b@q8_0`) — but never arbitrary substrings (so it will not be routed to an unrelated `...-abliterated` build).
- **Smart Model Matching**: Robust matching that handles common variations like `:latest` tags and case-insensitivity. For example, a request for `llama3` will correctly match `llama3:latest` on the backend.
- **Model Control**: Load and unload models on connected backends (Ollama and LM Studio 0.3.6+) directly from the web UI or the admin HTTP API — without touching the backend servers themselves.
- **Parallel Processing**: Unlike basic proxies, `ollamaMQ` can process multiple requests simultaneously (one per available backend), significantly increasing throughput for multiple users.
- **Aggregated Model Listings**: `GET /api/tags` and `GET /v1/models` are answered by the proxy from **every** compatible backend at once, not routed to one of them. Each backend's own listing is merged (so per-model metadata like `size`, `digest` and `details` is preserved exactly as that backend reported it) and deduplicated by name/id. Clients therefore see everything the proxy can route to, and the list no longer changes from one poll to the next depending on which backend happened to answer.
- **Backend Health Checks**: Automatically monitors backend status every 10 seconds. All backends are probed **concurrently**, each request bounded by its own 5-second timeout, so one unresponsive backend can't hold up everyone else's status. Probes cover both API type (Ollama vs OpenAI) and the list of currently available models (via `/api/tags` and `/v1/models`). Endpoints a backend rejects are remembered and skipped until re-checked, so incompatible backends don't spam warnings. Offline instances are temporarily skipped and marked in the dashboard.
- **Fail-Fast for Unsatisfiable Requests**: If no online backend can ever serve a queued request (wrong API family or model absent everywhere), it is answered with `503` after `stuck_timeout` seconds instead of hanging forever. Requests merely waiting for a busy or loading backend are unaffected.
- **Per-Model Concurrency Limits**: Each model entry's `max_concurrent_requests` caps how many in-flight requests one backend may serve for that model, bounded globally by `settings.max_concurrent_per_backend` (default 1 — the historical one-request-per-backend behavior; raise it to let a single backend handle several requests at once). The limit is keyed on the model's base name, so it applies no matter how the client spells it — `qwen3.8-27b`, `qwen/qwen3.8-27b` and `unsloth/qwen3.8-27b@q8_0` all draw on the same budget as the configured entry.
- **Metadata Reads Never Queue**: `/`, `/api/tags`, `/api/ps`, `/api/version`, `/api/show`, `/v1/models` and `/v1/models/{model}` don't run inference, so they neither wait for a free backend slot nor occupy one — a chat UI polling `/api/tags` on a timer is answered immediately instead of queueing behind a multi-minute generation (or a model load) with the default `max_concurrent_per_backend: 1`.
- **Per-User Queuing**: Each user (identified by a client API key, else by the `X-User-ID` header, else by address as `ip-<address>`) has their own FIFO queue, capped at 100 waiting requests — beyond that the proxy answers `429` rather than growing the queue.
- **Bounded Memory**: A queued request holds its whole body until a backend takes it, so bodies are capped two ways: `settings.max_body_bytes` per request (default 64 MiB, `413` beyond it) and `settings.max_queued_bytes` across all queues at once (default 512 MiB, `503` beyond it). A request is always admitted when the queues are empty, so an oversized body can never lock the proxy out of making progress. `/api/blobs/{digest}` keeps a separate 1 GiB allowance for model-layer uploads.
- **Fair-Share Scheduling**: Prevents any single user from monopolizing all available backends. A user's turn is decided by their **recent** load: every dispatched request adds to a score that halves every 5 minutes, so a long-idle user isn't punished for past traffic and a newcomer can't monopolize the proxy while "catching up". Requests are charged when dispatched regardless of outcome, so a client whose requests always fail or disconnect can't keep jumping the queue.
- **Cache-Aware Hold-Back**: On a backend that reports KV-cache telemetry (llama.cpp/halogen with `--metrics`), a conversation whose last turn mostly *missed* that cache — and whose estimated next-turn prefill does not fit the free KV — lines up behind the conversations served most recently instead of forcing a context switch and a full re-prefill every round. A session that just finished a turn also keeps queue priority for its follow-up for ~5 s (adaptive: 1.5x the session's own worst recent pause, 2–30 s), so an agent runs its whole cycle on its backend undisturbed; at most 64 reserved wins in a row, and any held request yields at most 16 times, so neither mechanism can starve anyone. Plain Ollama (no telemetry) keeps the plain fair order.
- **Sticky Routing (Session Affinity)**: A conversation goes back to the backend that served its previous turn, so a long multi-turn prompt is not re-prefilled on a backend with no KV slot for it. The key is an explicit session id when the client sends one, else a fingerprint of the conversation's first turn (see [Sticky Routing](#-sticky-routing-session-affinity)).
- **Session Labels & Priority**: A client can name its conversation (`X-Session-Name`) so it reads as `#refactor-auth` instead of `#a3f19c`, and ask for it to jump the queue (`X-Session-Priority: high|top`) as far as `settings.client_priority` allows. The operator can pin any live session from the icon beside it or from the API. `top` goes first on the whole board and may overbook its own backend by one slot (see [Session Labels & Priority](#-session-labels--priority)).
- **Transparent Header Forwarding**: Full support for all HTTP headers (including `X-User-ID`) passed to and from the backend, ensuring compatibility with tools like **Claude Code**.
- **VIP & Boost Modes**: Absolute priority (VIP) or increased frequency (Boost) for specific users.
- **Prometheus Metrics**: `GET /metrics` reports requests, outcomes, tokens, bytes and latency **per client and per model**, plus live queue and backend state — including `ollamamq_cache_waste_seconds_total`, the prefill time lost re-evaluating a conversation's context after it was evicted from a backend's KV cache (the waste share is `rate(cache_waste_seconds_total) / rate(prompt_seconds_total)`; only backends reporting per-request prefill telemetry contribute).
- **Web Dashboard**: live backend health, active requests, queue depths, throughput and the request log in a browser at `/ui` behind a sign-in page — plus model control and management of clients and their API keys.
- **Clients & API Keys**: Give each client one or more keys (generated or any string you choose). A request with a client key queues under the client's name, so a key holder can't hop queues by changing `X-User-ID`.
- **OpenAI Compatibility**: Supports standard OpenAI-compatible endpoints.
- **Async Architecture**: Built on `tokio` and `axum` for high concurrency.

## 🛠️ Installation

Ensure you have [Rust](https://rustup.rs/) (2024 edition or later) and [Ollama](https://ollama.ai/) installed.

### Option 1: Install via Cargo (Recommended)

```bash
cargo install ollamaMQ
```

### Option 2: From Source

1. Clone the repository:

   ```bash
   git clone https://github.com/Chleba/ollamaMQ.git
   cd ollamaMQ
   ```

2. Build and install locally:
   ```bash
   cargo install --path .
   ```

## 🏃 Usage

### Docker Installation

#### Using Docker Compose (Recommended)

1. Ensure Docker and Docker Compose are installed.
2. Start your local Ollama instance (defaulting to `localhost:11434`).
3. Run:
   ```bash
   docker compose up -d
   ```

#### Using Docker CLI

Pull the published release image:

```bash
docker pull ghcr.io/dmarchevsky/ollamamq:latest
```

Or build the image from the local Dockerfile:

```bash
docker build -t ollamamq:local .
```

Then run the container:

```bash
docker run -d \
  --name ollamamq \
  -p 11435:11435 \
  --restart unless-stopped \
  ghcr.io/dmarchevsky/ollamamq:latest
```

### API Proxying

Point your LLM clients to the `ollamaMQ` port (`11435`) and include the `X-User-ID` header.

#### Supported Endpoints:

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


#### Example (cURL):

```bash
curl -X POST http://localhost:11435/api/chat \
  -H "X-User-ID: developer-1" \
  -d '{
    "model": "qwen3.5:35b",
    "messages": [{"role": "user", "content": "Explain quantum computing."}],
    "stream": true
  }'
```

#### Model aliases and Olla-style paths

`settings.model_aliases` lets clients ask for a model under another name: a request whose `model` matches an alias (any case) is rewritten to the target before routing, so the model policy, logs and metrics all see the real name. Olla-style URLs (`/olla/openai/v1/...`, `/olla/proxy/api/...`) are accepted too and served as the plain routes, so a client set up for Olla only needs a new port. Only known routes are served behind those prefixes.

#### Decision-model backends (System One)

A third backend family: "decision model" servers — `clm-serve` ([CLM](https://github.com/Contrastive-LM/CLM)), `laya-serve` ([Laya](https://github.com/NandhaKishorM/laya)), `kev.serve` ([Kev](https://github.com/jaredpalmer/kev)) — which answer `POST /v1/systemone` with typed decisions (`choice` / `score` / `noul`) instead of text. The body is passed through byte-for-byte (never parsed), and it goes **only** to backends of this family: these servers implement **no `/api/*` route at all**, so `/api/chat`, `/api/tags` and `/v1/chat/completions` are never sent to them, and they never answer a route their family cannot serve — such a request is refused, not misrouted. Only `/v1/systemone` is proxied; `/v1/rank` and Kev's `/v1/systemone/permute|separate` are not.

- **Detection** — a backend answering `GET /v1/models` with `{"models":[{"name":…}]}` (CLM, Kev) is identified as System One; a 2xx `GET /health` marks a backend online when nothing else identifies it. Laya exposes no listing endpoint and therefore cannot be positively detected — pin it with `kind: systemone` on the backend entry. Auth on all three is `Authorization: Bearer <key>`, which is exactly what the existing `token:` key already sends (`CLM_API_KEY` / `LAYA_API_KEY` / `KEV_API_KEY`).
- **Model names** — CLM and Kev are listed by the probe automatically. A Laya server has nothing to list, so its checkpoints must be declared under `models:`: there, a `models:` entry makes a name **routable**, not merely pinned, and a `models:` entry with no `backends:` selector is admitted on *every* backend — use an explicit selector as soon as you run more than one backend.
- **Listings** — decision-model names appear in the aggregated `GET /v1/models` **and** `GET /api/tags` even though only `/v1/systemone` can serve them. That is deliberate: ollamaMQ aggregates one namespace and routes by capability. `/v1/models` always answers in OpenAI's `data`/`id`, `/api/tags` always in Ollama's `models`/`name`; names for a backend that cannot list anything are synthesised from proxy state and appended after the live entries, so real metadata (`size`, `digest`, `description`) always wins.
- **Model control is not available** for this family — see [Model Control](#model-control-admin-api).
- ⚠️ **`laya-serve` binds `0.0.0.0:8000` and `clm-serve` binds `0.0.0.0:8700` open by default** — anyone on the LAN can run inference against them. Bind them to loopback or set their API key. `kev.serve` binds `127.0.0.1:8008`.
- `clm-serve` needs a separate vLLM pooling encoder (default `http://127.0.0.1:8090/v1/embeddings`) and answers `502` while that is down. That is CLM replying, not ollamaMQ declaring the backend dead.

```bash
curl -X POST http://localhost:11435/v1/systemone \
  -H "X-User-ID: developer-1" \
  -d '{"model": "clm-latest", "state": "billed twice, refund please or we cancel",
       "questions": {"refund": {"type": "noul", "instructions": "Is a refund justified?"}}}'
```

### Model Control (Admin API)

Models can be loaded and unloaded on connected backends without touching the backend servers. Supported backends:

- **Ollama** — no dedicated endpoint exists; loading is a `POST /api/generate` with an empty prompt (the scheduler loads the model and returns immediately), unloading is the same call with `keep_alive: 0`.
- **LM Studio 0.3.6+** — the native `POST /api/v1/models/load` / `POST /api/v1/models/unload` endpoints (unload targets a specific loaded instance). Older LM Studio versions are detected and a friendly error points at the `lms load`/`lms unload` CLI.
- Plain OpenAI backends (vLLM, etc.) have no control API and are rejected with a clear error.
- **System One decision-model servers (CLM, Laya, Kev)** have no model-control API either — no load/unload, no `keep_alive`, not even `/api/ps`. Residency is configured on the server itself (e.g. `LAYA_PRELOAD`, `LAYA_MAX_LOADED`), so a `models:` entry for these backends only makes the name routable and pins it; it never starts a load.

All three endpoints are served by the proxy itself (they are never proxied to a backend). They are **not** open by default: an admin call is accepted from

1. the proxy key, when `OLLAMA_MQ_API_KEY` is set (`Authorization: Bearer <key>` or `X-API-Key`),
2. otherwise the dashboard's own credential — a live session cookie, or the same user and password as HTTP basic auth (`curl -u admin:…`). The cookie is set with `Path=/ui`, so a browser will not offer it to `/admin/*` on its own; pass it yourself (`curl -b omq_session=…`). This works whether or not the dashboard is listening on this interface: the credential is what counts, so an operator can keep `settings.web_ui.listen` on `127.0.0.1` and still administer over the network,
3. otherwise — and only when neither of the above exists — any **enabled** client key from `keys.toml`.

Anything else gets `403` with a body naming what to configure, and the startup log says which door is open (or that the control is closed). A client key is the last resort rather than the rule: a box full of client keys is not a box whose clients may reconfigure it. The dashboard's own model controls go through `/ui/api/*` and follow the dashboard's login.

#### `GET /admin/models`

Per-backend inventory: index, URL, online status, detected API type, LM Studio flag, active request count, available models, currently loaded models, and any in-flight control operation.

```bash
curl -s http://localhost:11435/admin/models | python3 -m json.tool
```

#### `POST /admin/models/load` and `POST /admin/models/unload`

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

### Logging

`ollamaMQ` uses [`tracing`](https://docs.rs/tracing). Logs go to **stderr** at a default level of `debug`, so you see everything — backend health checks, per-request routing, backend detection, and errors. That makes the systemd journal, the Docker log, or a piped file the only log you need; there is no separate log file.

Override the level at any time with the standard `RUST_LOG` environment variable:

```bash
# Quieter console (info + errors only)
RUST_LOG=info ollamaMQ

# Everything, including per-request dispatcher detail
RUST_LOG=debug ollamaMQ
```

> `--no-tui` is still accepted and does nothing: it used to turn off the terminal
> dashboard, which is gone. When it is passed the proxy logs one `WARN` line at
> startup, so drop the flag from your command line whenever convenient.

### Running as a systemd Service

To run `ollamaMQ` as a background service with full log visibility in the journal:

1. Copy the provided service file to your system's unit directory:

   ```bash
   sudo cp ollamamq.service /etc/systemd/system/
   ```

2. Edit the file and set the correct `ExecStart` path to your installed binary (e.g. `~/.cargo/bin/ollamaMQ`) and your backend URLs:

   ```ini
   ExecStart=/usr/local/bin/ollamaMQ --port 11435 --backend-urls http://localhost:11434
   ```

   > **Note:** If you installed `ollamaMQ` with `cargo install`, the binary is at `~/.cargo/bin/ollamaMQ`, **not** `/usr/local/bin/ollamaMQ`. Point `ExecStart` at the real path, or create a symlink so the default path works:
   >
   > ```bash
   > # Find the real path
   > which ollamaMQ
   >
   > # Option A: edit ExecStart to use the real path
   > # Option B: symlink it to the expected location
   > sudo ln -s "$HOME/.cargo/bin/ollamaMQ" /usr/local/bin/ollamaMQ
   > ```

3. Reload systemd, then enable and start the service:

   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable --now ollamamq
   ```

4. Follow the logs in real time via the journal:

   ```bash
   journalctl -u ollamamq -f
   ```

   Or show the most recent 100 lines:

   ```bash
   journalctl -u ollamamq -n 100
   ```

Because logs default to `debug` on stderr, **all** dispatcher events (backend health, request routing, errors) are captured in the journal — no separate log file needed.

## ⚙️ Configuration (`appconf.yaml`)

Everything about your `ollamaMQ` instance lives in one YAML file with three sections: **backends** (what to connect to), **settings** (runtime options) and **models** (which models to load where). The default path is `appconf.yaml` in the working directory; override it with `-c/--model-config`. A missing file is fine — defaults apply.

```yaml
# --- Backends: every Ollama / LM Studio / OpenAI-compatible server you want to use
backends:
  - http://10.137.1.1:11434      # plain URL — no auth
  - url: http://10.137.1.2:1234   # object form adds an optional API token,
    token: "some-api-key"         # sent as `Authorization: Bearer <token>`
  - url: http://10.137.1.3:8000   # and can pin the API family for a backend
    kind: systemone               # nothing can detect (a Laya-style server)

# --- Runtime settings (all optional — defaults shown)
settings:
  port: 11435                  # proxy listen port (default 11435)
  host: 127.0.0.1              # bind interface; use 0.0.0.0 for LAN/Docker access (default 127.0.0.1)
  timeout: 300                 # per-request timeout in seconds (default 300)
  load_keep_alive: 86400       # how long model-control loads stay resident; -1 = forever (default 86400)
  allow_all_routes: false      # also proxy non-standard endpoints as fallback (default false)
  stuck_timeout: 60            # fail-fast 503 after N s when no backend can ever serve a queued request (default 60)
  max_concurrent_per_backend: 1  # global cap of in-flight requests per backend; raise to let one backend handle several at once (default 1).
                                 # Metadata reads (/api/tags, /api/ps, /api/show, /v1/models, ...) are exempt: they
                                 # neither wait for a slot nor take one.
  max_body_bytes: 67108864       # largest request body accepted, 413 beyond it (default 64 MiB).
                                 # /api/blobs/{digest} keeps its own 1 GiB allowance.
  model_aliases:                 # serve a model under another name (case-insensitive), applied before routing,
    codex-auto-review: Qwen3.8-Flash-Next   # the model policy and metrics; reloaded with the config (default none)
  ip_identity: true              # requests with no client key and no X-User-ID queue as ip-<address>;
                                 # false = they all share "anonymous" (default true)
  sticky_routing: prompt         # keep a conversation on the backend that served its previous turn:
                                 # off | header | prompt (default) | identity — see Sticky Routing
  sticky_ttl: 1800               # seconds idle before a conversation may move (default 1800)
  sticky_lag: 0                  # extra requests its backend may run vs the emptiest eligible one
                                 # before the conversation moves anyway (default 0)
  sticky_max_entries: 10000      # conversations remembered; the idlest are forgotten (default 10000)
  client_priority: global        # how far a client's own X-Session-Priority claim reaches:
                                 # off (operator only) | self (high, within its own client) |
                                 # global (high and top anywhere). Keyless clients never get a
                                 # say, and a pin set in the UI/API always wins (default global)
  detect_backend_slots: true     # use the parallel slots a backend reports (llama.cpp /props total_slots,
                                 # halogen /health slots) as its capacity instead of max_concurrent_per_backend;
                                 # models without max_concurrent_requests may then use all of them (default true)
  max_queued_bytes: 536870912    # total bytes of request bodies waiting in all queues, 503 beyond it
                                 # (default 512 MiB); a request is always admitted when the queues are empty
  request_log_path: ollamamq-requests.jsonl   # conversation JSONL log file (default shown)
  request_log_max_bytes: 10485760             # rotate when the file exceeds this (default 10 MiB)
  request_log_max_files: 5                    # rotated files kept as .0 .1 ... (default 5)
  request_log_flush_secs: 5                   # quiet time after which a conversation is written out (default 5)
  log_content_limit: 1048576                  # max body bytes captured per request, min 1024 (default 1 MiB)
  clients_file: clients.yaml   # clients and their API keys, managed from the web UI (default shown)
  models_file: models.yaml     # model filter rules, managed from the web UI (default shown)
  metrics_token: ""            # bearer token for GET /metrics; empty = open (default)
  web_ui:                      # web UI at /ui; off unless a password or hash is set (here or in OLLAMA_MQ_UI_PASSWORD)
    username: admin            # sign-in user (default admin)
    password: change-me        # first-run sign-in password
    # password_hash: ""        # argon2id hash; the UI's "Change password" writes it here and it
                               # wins over any plaintext password (delete it to fall back)

# --- Models to load on startup / reload, via the model-control logic
models:
  - name: "gpt-oss:120b"        # model name as the backend knows it
    identifier: "my-gpt"        # label attached to this entry's load op (dashboard / admin API)
    max_ctx: 128000             # context window — Ollama num_ctx / LM Studio context_length
    keep_alive: 86400           # seconds the model stays resident after load (Ollama; -1 = forever)
    max_concurrent_requests: 3  # in-flight requests allowed for this model on one backend (default 1)
    backends:                   # which backends to load it on — every backend whose URL contains the string is
                                #   targeted, so `- :11434` covers all backends on that port. A non-empty list also
                                #   PINS routing for this model (see below); omitted/empty = any suitable backend
      - http://10.137.1.1:11434
```

> **Upgrading:** `max_body_bytes` replaces a flat 1 GiB limit that applied to every route. If you push bodies larger than 64 MiB through anything other than `/api/blobs/{digest}`, raise it.

**How it's applied:**

- `backends` and `settings` are read at **startup only** — restart to change them.
- A backend's optional `token` is sent as `Authorization: Bearer <token>` on every request the proxy makes to that backend — proxied inference, model listings/metadata reads, health probes and model load/unload. It's meant for backends behind an authenticated reverse proxy (LM Studio, vLLM, …). A client's own `Authorization`/`X-Api-Key` header is always forwarded untouched: the token only fills in when the request carries no credentials of its own.
- `models` is applied automatically at startup (once backend probes have run) and re-applied any time you click **Reload config** in the web UI. Application is additive: each target endpoint is checked live first (Ollama `/api/ps`, LM Studio loaded instances), so models already resident there — e.g. still loaded from an earlier run because of long `keep_alive` — are skipped instead of being loaded twice; everything else gets a load started. It never unloads anything, and loads for the same backend run one at a time (backends reject parallel control ops). Every attempt, including skips, is reported in the dashboard Logs panel (`⟳ CTL`) and in the log output.
- A backend won't accept a load while it is serving requests, so each entry **waits for that backend to go idle** (up to 5 minutes) before loading. Without this, restarting the proxy under live traffic meant every configured model was refused on the spot and never retried — leaving models unloaded, or loaded with the wrong `max_ctx`.
- Explicit CLI flags override file values when given (e.g. `--backend-urls` over `backends`, `--port`/`--host`/`--timeout` over `settings`).
- A non-empty `models[].backends` list also **pins routing**: requests for that model are only sent to the listed backends (case-insensitive substring URL match, the same rule as load targeting) and never to other suitable ones. If none of the pinned backends is online/eligible, the request fails with HTTP 503 after `stuck_timeout` (error: `no configured backend available for model '<model>'`) instead of being routed elsewhere. If `backends` is empty/omitted, or the model is not in the config at all, the existing behavior applies — any online backend that lists the model may serve it.
- A `models:` entry does two things: it **loads** the model where the backend supports control, and it makes the name **routable** on the selected backends even where nothing can list it. The second half is the only way to reach a decision-model server with no listing endpoint (Laya), and it applies to backends without model control as well — for them the entry names and pins, it does not load.

See [`appconf.yaml.example`](appconf.yaml.example) for a fully commented template.

## Request logging

The log keeps **one line per conversation**, not one per request. A chat client that sends twenty turns asks the same history twenty times over, and one line per request would store that repetition twenty times; a conversation is therefore one JSON object that grows in place and is appended again when it has more to say. `settings.request_log_path` (default `ollamamq-requests.jsonl`) holds these lines:

| field | meaning |
| --- | --- |
| `v` | record format version — `2` |
| `key` | the store key: `client#session` for conversation traffic, `client·rid` for a request with no conversation tag (one such request is one entry) |
| `user`, `model`, `backend`, `priority` | who and what, as of the newest turn |
| `session`, `session_label` | the sticky-routing conversation tag and the label the client sent (`X-Session-Name`) |
| `start_ms`, `end_ms` | the first turn's arrival and the newest turn's last event, unix ms |
| `redacted` | `true` when any turn had body capture switched off |
| `dropped_turns` | turns trimmed from the front after a conversation hit the 500-turn cap |
| `turns[]` | the requests, oldest first — below |

Each turn is one proxied request and its response:

| field | meaning |
| --- | --- |
| `rid` | correlation id, shared by a request and its response |
| `at_ms` | when the request arrived, unix ms |
| `tl` | `queue`, `dispatch`, `first_token`, `end` — unix ms, stamped at the moment each happens. Stages a turn never reached are absent, so a gap in `tl` is a fact and not a guess |
| `method`, `path`, `status`, `outcome`, `model`, `backend`, `priority` | the request line, its answer, and how it ended (`ok`, `timeout`, `client_gone`, `dropped`, …) |
| `req`, `resp` | `{bytes, chars, truncated}` per direction — sizes, never text |
| `prompt_tokens`, `completion_tokens`, `waste_secs` | usage as reported, plus prefill seconds lost to a KV-cache eviction |
| `msgs[]` | one entry per request message — `role`, `chars`, and `name`/`calls` when it carries a tool name or issues tool calls. Roles and sizes, never text |
| `redacted` | body capture was switched off for this turn |
| `content` | `req`/`resp` captured text, cut to `log_content_limit` with a `...[truncated: N bytes total]` marker; absent when nothing was stored |

**When a line is written.** A conversation is appended once it has been quiet for `request_log_flush_secs` (default 5 s — a crash should cost the last few seconds of a conversation, not the last minute; nothing is `fsync`-ed, so a hard power loss can still take what the operating system had not written), and a longer line follows if it resumes, so **the last line for a `key` is the complete one** — readers take that and ignore earlier lines of the same key. Earlier lines of the same key are therefore *superseded snapshots*, not separate conversations: each one contains what the conversation had up to that point, which is why consecutive lines overlap (the newer response of line 1 appears again inside line 2, because the client resent it). Appending is kept instead of rewriting in place so a crash mid-flush costs at most the last line. Folding the file by `key` and keeping the longest line per key yields exactly **one record per conversation**. At startup the log re-seeds from the file's tail, so open conversations survive a restart without reading the whole file. The in-memory log keeps the 200 most recent conversations and at most 64 MiB of captured text.

**Where the file goes, and what happens if it cannot be opened.** Missing directories inside `request_log_path` are created at startup. A path that still cannot be opened — a container path such as `/data/…` on a host without one — falls back to `ollamamq-requests.jsonl` next to `appconf.yaml`, the store folder the other state files live in, and the log says so (`conversation log: <configured> is unusable here, writing to <actual> instead`) instead of starting with capture off and leaving every log row unopenable. Capture applies from boot: changing the path in Settings takes effect on restart. The Logs page shows the path being written to, and when nothing could be opened it says so above the table and badges the rows `not captured`.

**Log capture.** Every client, every API key and every backend carries a `redact_log` switch (web UI checkbox, `redact_log: true` in `clients.yaml` / `appconf.yaml`). When it is on, the request is served exactly as before and the turn is still written — timeline, sizes, message roles, status, token counts — with no body text, so `content` is absent and `redacted` is `true`. Nothing about the traffic changes: queues, priority, metrics and the response the client sees are untouched. Only *new* requests are affected; lines already in the file keep what they stored. The web viewer renders those conversations in full — turn blocks, gaps and timeline — and says *capture turned off* where the text would be, instead of pretending the structure is missing.

- A **client** rule covers its requests in both directions and every key of that client.
- A **key** rule covers that key's requests in both directions. Redaction is an opt-out: a key whose client is marked stays covered whatever the key says.
- A **backend** rule covers the **response** text of requests it served. A request's own text is logged before a backend is chosen, so that half is the client/key's job.

**Rotation:** when the file exceeds `request_log_max_bytes` it is renamed to `.0` (existing `.0` → `.1`, etc.), at most `request_log_max_files` rotated files are kept and the oldest is deleted. The current file size is re-read at startup, so sizing/rotation survives restarts. When a rotation cannot happen at all (a read-only mount), the writer says so and keeps appending to the current file, tracking its real size so the ceiling stays honest rather than silently resetting to zero.

**Web UI:** the **Logs** page shows the newest live log events (one line per request and per response, from the in-memory ring); clicking a row opens the conversation viewer. It is independent of the file format above.

> **Note:** request/response headers are deliberately **not** logged — this keeps API keys out of the file.

## 🐳 Docker

Release images are published to GHCR as
[`ghcr.io/dmarchevsky/ollamamq`](https://github.com/dmarchevsky/ollamaMQ/pkgs/container/ollamamq)
(tags: `latest`, `<version>`, `<major>.<minor>`, pushed automatically on every release tag).
The old Docker Hub image (`dmarchevsky/ollamamq`) stopped receiving builds at `0.9.1` — installs
that still pull it need to switch to the GHCR name.

### Docker Compose

The included [`docker-compose.yml`](docker-compose.yml) keeps everything in one host folder, with relative paths only:

```
ollamamq/
├── docker-compose.yml
├── config/   → /config   appconf.yaml, .env, clients.yaml, models.yaml
└── logs/     → /data     ollamamq-requests.jsonl* (the service log: docker logs ollamamq)
```

1. Copy `docker-compose.yml` into an empty folder (or run it in this checkout).
2. Create `config/appconf.yaml` and `config/.env` (`OLLAMA_MQ_UI_PASSWORD=…`).
3. Run `docker compose up -d --build`.

Set `build.context` to where the source is:
- `.` in this checkout
- `./src` for a clone inside the server folder
- `https://github.com/dmarchevsky/ollamaMQ.git#master` to build straight from GitHub (pushed commits only)

**Backends on the Docker host:** use `http://host.docker.internal:<port>` in `appconf.yaml`. On Linux, the server must listen on `0.0.0.0` (for Ollama: `OLLAMA_HOST=0.0.0.0`), and the firewall must let the Docker bridge reach that port.

**Moving to another server:** stop the service and copy the folder. The files in `config/` are written by the container as root with mode 600, so copy them with `sudo` (or `docker run --rm -v "$PWD":/s alpine tar …`).

**Where store files go:** relative paths for `clients_file` and `models_file` are resolved from the folder that holds `appconf.yaml`, so by default they sit next to it. Logs (`request_log_path`) stay relative to the working directory — except that a conversation log whose configured path cannot be opened lands in the `appconf.yaml` folder (see [Request logging](#request-logging)).

Older setups also had a `settings.json` (the web UI's overlay) and an `admin.json` (the password changed in the UI). At startup their values are merged into `appconf.yaml` once and both files retire to `.bak`.

Earlier versions kept the store files in the working directory. If such a file exists there and not next to `appconf.yaml`, it keeps being used, and the startup log says so. To switch, move it next to `appconf.yaml`.

### Dockerfile

The Dockerfile uses a multi-stage build:

- **Build stage**: Uses `rust:1.97-alpine` (pinned) to compile the release binary
- **Runtime stage**: Uses `alpine:3.20` with only `ca-certificates` for a minimal footprint (~10MB)

### Environment Variables

| Variable       | Description                                        | Default                  |
| -------------- | -------------------------------------------------- | ------------------------ |
| `BACKEND_URLS` | URLs of the backend servers (Ollama, LM Studio, …) | `http://localhost:11434` |
| `OLLAMA_URLS`  | Legacy alias for `BACKEND_URLS` (used only when `BACKEND_URLS` is unset) | — |
| `PORT`        | Port for ollamaMQ to listen on | `11435`                  |
| `HOST`        | Host/interface to bind to      | `0.0.0.0`                |
| `TIMEOUT`     | Request timeout in seconds     | `300`                    |
| `OLLAMA_MQ_API_KEY` | Optional API key; enables auth when set | *(unset — auth disabled)* |
| `OLLAMA_MQ_UI_USER` | Web UI sign-in user (overrides `settings.web_ui.username`) | `admin` |
| `OLLAMA_MQ_METRICS_TOKEN` | Bearer token required by `GET /metrics` (overrides `settings.metrics_token`) | *(unset — open)* |
| `OLLAMA_MQ_UI_PASSWORD` | Web UI sign-in password (overrides `settings.web_ui.password`); enables the web UI. A `settings.web_ui.password_hash` wins over it | *(unset — web UI off)* |

### 🔒 Security (Optional API-Key Auth)

By default, `ollamaMQ` only listens on loopback (`127.0.0.1`), so it is only reachable from the local machine. If you expose it to the network with `--host 0.0.0.0`, set the `OLLAMA_MQ_API_KEY` environment variable to a secret key to protect the proxy. Auth is **opt-in**: if the variable is unset or empty, behavior is unchanged and no key is required.

When a key is set, every request **except `/health`** must present it via one of two headers:

- `Authorization: Bearer <key>`
- `X-API-Key: <key>`

The key of any enabled client (see *Web UI, Clients & API Keys* below) is accepted too. Requests with a missing or wrong key get `401 Unauthorized` (`{"error":"unauthorized"}`). The `/health` endpoint always stays unauthenticated so health checks and Docker healthchecks keep working.

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

### 🖥️ Web UI, Clients & API Keys

Set a password and open `http://<host>:11435/ui/`:

```bash
OLLAMA_MQ_UI_PASSWORD=change-me ollamaMQ     # user defaults to "admin"
```

The web UI is **off** unless a password is configured (`OLLAMA_MQ_UI_PASSWORD`, or `settings.web_ui.password` in `appconf.yaml`). Browsers sign in on `/ui/login`, which sets a session cookie (`HttpOnly`, `SameSite=Strict`, `Secure` when the proxy in front sends `X-Forwarded-Proto: https`) valid for 7 days; **Sign out** is in the sidebar. Sessions are kept in memory, so restarting ollamaMQ signs everyone out. After 10 failed sign-ins within 10 minutes further attempts get `429` until the window passes. Scripts can skip the page and send the same user and password as HTTP basic auth to `/ui/api/*`. This is all independent of `OLLAMA_MQ_API_KEY`. The password crosses the network at sign-in, so put the UI behind TLS (a reverse proxy) when it is reachable from beyond a trusted network.

The dashboard covers:

- **Overview** — status at a glance over a selectable interval (default 15 min, up to 24 h — see below)
- **Backends** — health, API type, load, and per-backend model lists with **Load** / **Unload**; also where backends are configured: **Add** (with **Test connection**), name, token, **Disable/Enable**, **Remove**, and whether the backend's response text is stored in the log (see [Request logging](#request-logging))
- **Clients** — every queue identity with its queue depth and counters; **VIP**, **Boost**, **Block**, **Block IP**, **Unblock**. Registered clients are two levels: open a row to edit or delete its keys
- **Models** — every model the backends list, with a switch per model and filter rules (below)
- **Blocked** — disabled clients, blocked ids and blocked IPs
- **Logs** — recent log events newest-first, with a switch between the two ways to read them. **By conversation** (default): one row per conversation — what a session did, instead of twenty rows that each repeat part of the last one. The row names the client and the session as its client named it (`#cache-warmup`, or the six-hex tag when nothing labelled it) and sums the traffic in an Activity line (`→ 4 req · 1.3 MB ← 200 · 92 KB`, with `waiting` or `2 failed` when either is true). **By request**: one row per request round (`→ request · ← response`), which is what you need when a single call misbehaved. In both views a CTL action and traffic that sent no session token stand alone as their own rows, and a round whose body text was switched off is marked `no text`. The choice is remembered like the other view choices. Clicking a row opens the **conversation viewer** on that conversation — a grouped row lands on its newest request, a per-request row on the request clicked: every turn of the conversation as a block on one timeline, each headed by what was measured — `queued 0.6s · prefill 6.4s · gen 11.0s · 31.4 t/s` — with any quiet stretch of 5 s or more between two turns called out as `idle 55s · tool read (10:38:07 → 10:39:02)`, because those are the tool results the next request carries. The focused turn is expanded: its messages as a transcript (thinking collapsed, tool calls rebuilt, token footer), showing only what the request added. Everything it merely re-sent collapses into one line — `▸ 7 messages held from request 1 · system 1 · user 2 · assistant 2 · tool 2 · ≈799 B — show them here too` — because the turns above already drew those messages; click the line to see them inside this request as well. The request line counts both halves: `10 messages · 3 new this turn`. Other turns stay as header lines until opened. A streamed response is read back as the answer it produced, not as the wire it came over: `Formatted` reassembles the SSE (or Ollama line-JSON) chunks into one assistant message — text, collapsed thinking, rebuilt tool calls, finish reason and usage — and drops keep-alive comments, empty deltas and the `[DONE]` marker; `Raw` shows the captured bytes as stored. A stream that opens with `: keepalive` is still a stream. `⏮ ‹ › ⏭` walk request by request (the one you clicked from is where it starts), `⟳` reloads to pull in turns that arrived since, and `⤓ Download` saves the whole conversation — Formatted gives a markdown transcript, Raw gives the log line as the file holds it. `≡ Jump` opens the request tree — drag its left edge to make it wide or narrow (double-click, or focus the edge and use `←`/`→`, to reset), and the width is remembered like the other viewer choices — request → request/response → message → tool call, each request folding on its own — and it is the *same* fold as the request's header in the body: closing it in the tree closes the block, closing the block closes the tree, and `Expand all` / `Collapse all` move both. A request row reads `3 new · 7 held` and lists only the messages that request added — the re-sent ones live under their own request, and the body's one-line history is where you open them. It is stamped the way `dmesg -H` stamps a log: the conversation's first request owns the absolute clock (`17:30:14`), and every stamp after it counts **continuously from the conversation's start** — request 2 at `+18s`, request 3 at `+31s`, never the same number twice — and the one rule for the whole column is **`+` means a moment, a plain number means a length** — a request row is a moment (`+31s` after the conversation began), a response row is how long that response took (`18.4s`), an idle row how long it sat quiet (`6.2s`), a message row the window that produced it. Hover any of them and it says which it is. There is no third kind: a `TOOL` result is the stretch between the previous answer ending and this request arriving (`2.0s`, tooltip `produced between 14:53:02 and 14:53:57`), an `ASSIST` row is the generation that wrote it. The absolute time of any later row stays one hover away. `SYS` and `USER` rows stay blank and say why on hover: they arrive inside the request, so they have no instant of their own, and inventing one per line would be a precision the capture never measured. Clicking a request row focuses it in the body; clicking a message row (`USER`, `ASSIST`, `TOOL`) jumps to that message and highlights the row you are looking at — for a message the request merely re-sent, that is the earlier turn where it is drawn, unless this request has its held history unfolded, in which case the copy in front of you is the one you land on. Capture turned off is rendered as structure, not absence: the blocks, timeline, sizes and message roles are all there, with *capture turned off* where the text would be. The page behind never scrolls while the viewer is open — the only scrollbars are the log's own and the tree's. Truncated captures are salvaged rather than shown as raw.
- **Settings** — limits, model aliases, metrics token, the admin password, and the version you are running (see below)
- **Reload config** — re-read `appconf.yaml`

The **Overview** page shows:
- a one-line health summary
- a **Capacity** board: one proportional bar per backend — solid client-coloured segments for the running requests (width ∝ the request's estimated size) and clearly visible free slots; waiting requests get a slim row under the bar on the same token scale, so a backlog visibly passes the backend's end. Where a backend reports its KV cache (a llama.cpp server started with `--metrics`), the free space is sized by it; elsewhere free slots split the remainder equally
- a **Filters** button in the top bar: the modal picks the time interval (5/15/30/60 min presets or "last N minutes/hours" up to 24 h), whether clients are listed bare or as `client:key` (the API key label — lanes, rows and the Clients table then read e.g. `codex-laptop:gen`), and multi-select client, backend and model lists. The selection scopes every panel on the page — Queue, Running, Done, Capacity, Activity and both tables (free slots and backend health stay backend-wide, they are not anybody's) — and is remembered per browser; the button dots while anything deviates from the defaults
- an **Activity** panel. The graph is one row per conversation, grouped under the backend it ran on: each request paints one block per stage — **waiting** (queued for a slot, hollow blue), **prefill** (yellow), **decode** (green) — placed at the moment that stage began and as wide as it ran, so a 40s decode reads as forty seconds next to a 1s prefill. Colours mean the stage and nothing else; the client's own colour is the swatch beside its name, and identity hues deliberately avoid every status hue so a swatch can never be misread as a stage. A conversation that goes quiet leaves a gap on its row, and a stage cut off by an edge of the window is clipped there and marked `‹`. Hovering a lane lights the whole row in its client's colour. Below the graph the list threads each conversation: one block per conversation, one line per stage event with its own timestamp, plus an `idle` line where the conversation sat quiet. Graph and Details list conversations by when each session **started** (newest first), one shared order, so a running session never jumps around; **Active** shows running and waiting, **All** adds the recently finished, and **⊞ / ⊟** expand or collapse every conversation at once. The conversation header carries what a stage line cannot: request count, tokens, the span it covers, and — while it is active — the rates it is being served at: `decode 88.7 t/s` from the live stream and `prefill 349 tok/s` measured on the last finished request there (a request cannot time its own prefill: by the time the first token proves anything it is already decoding, so while the first token is still out the header shows `prefill 4.2s` and puts the last measured rate in its tooltip), or `waiting #2`. Each header ends with the same `≣ View` chip the stage lines below it carry — same 11 px face, same pill size, and the rows carry the header's right inset so the chips fall in one column instead of a ragged edge. Live lines wash in the client's colour, while the graph reserves that wash for hover. Clicking a graph row focuses that conversation in the list below. Sessions come from sticky routing: the client's `X-Session-Id` header, its `prompt_cache_key`, or a fingerprint of the conversation (see [Sticky Routing](#-sticky-routing-session-affinity))
- **Backends** and **Clients** tables with requests, errors, first-token p50/p95, generation and prefill speed, and token totals

Numbers cover the chosen interval and come from an in-memory history of the last 24 hours, newest 20 000 requests (finished requests, plus what is running or queued right now). The history starts empty on every restart. Request sizes are estimated as one token per four body bytes while queued or running — the exact prompt count only arrives with the finished response. Every stage line in **Details** and every conversation header carries a `≣ View` chip: either opens the conversation viewer — the stage line on that request, the header on the whole conversation. Because stage times are stamped where they happen, an `idle` line between two turns can name what ran inside it (`idle 2m 30s · tool edit`) instead of leaving you to infer it from durations.
- *First token* is measured from dispatch to the first generated token for streams, or is the prompt time the backend reports for non-streamed responses.
- *Generation speed* is per stream (median).
- *Share of slot time* is each client's part of the backend time used in the window, which is what fair scheduling divides.

#### Backends

The **Backends** page lists the configured backends and edits the backend list while the proxy runs (saved into `appconf.yaml`, same rules as Settings):
- **Adding** a backend probes it within 10 s. **Test connection** shows its type, slots and models before you add it.
- **Disabling** one stops new requests at once; running ones finish.
- **Removing** one disables it now and drops it from the list at the next restart.
- **Names:** a backend can have a **name** (`backends: - url: …` with `name: gpu-box` in `appconf.yaml`, or in the UI). The UI shows the name instead of `host:port`. Prometheus keeps `backend="host:port"` and adds `ollamamq_backend_info{backend, name}` for dashboards.
- **Log capture:** a backend can be marked **Do not save response text** (`redact_log: true` on its entry, shown as a `no text` badge in the list). Its requests still log an entry with status, size and timing — only the response text is left out. See [Request logging](#request-logging).
- **`--backend-urls`:** when given, the list is fixed and the page shows it read-only.

#### Settings

The **Settings** page edits `appconf.yaml` itself while the proxy runs.
- **The file is rewritten on save** — comments and custom formatting do not survive it; the previous version is kept as `appconf.yaml.bak`. The process must be able to write `appconf.yaml`.
- **Precedence:** command-line flags and environment variables > `appconf.yaml` > built-in defaults. A value set on the command line or in the environment shows as locked.
- **Source:** each setting shows where its value comes from, and **Use the default value** removes it from the file.
- **Undo last save** restores the previous `appconf.yaml`, kept as `appconf.yaml.bak` (undo again redoes).

**What applies at once:**
- **Live:** timeouts, slots and queue limits, model aliases, keyless IP identity, unknown-route proxying, and the metrics token.
- **After a restart:** largest request body.
- **Read-only:** listen address, log files and store paths. Set these in `appconf.yaml`.

**Admin password:**
- **Change password** stores an argon2id hash as `settings.web_ui.password_hash` in `appconf.yaml`.
- **Precedence:** the hash wins over `OLLAMA_MQ_UI_PASSWORD` and `settings.web_ui.password`. Those become the first-run password.
- **Sessions:** other sessions are signed out.
- **Forgot it?** Delete `password_hash` from `appconf.yaml` and restart; the configured password works again.

It also manages **clients and their API keys**. The Clients page is one list with two levels (filter with *All / Registered / Unregistered*): a row per id, and — while that row is open — the client's keys beneath it. *Expand all / Collapse all* works over the whole list.

- **Registered clients** are created in the UI and saved in `clients.yaml`. They authenticate with their API keys and are listed even when idle.
- **Unregistered ids** are what requests without a key are queued under: their `X-User-ID`, or the IP a front proxy fills in, else `anonymous`. The value is sanitised before it is used — clipped to 64 characters, control characters and runs of whitespace collapsed to `-`, and anything outside printable ASCII dropped — because it arrives from the client and is shown in the dashboard and in metrics labels verbatim otherwise. `:` is stripped so no caller can forge the namespace the proxy reserves for its own identities. The header itself still reaches the backend unchanged. They live in memory and drop off the list after an hour idle. **Register** turns one into a client of the same name and opens the add-key dialog.

- Create a client (its name becomes its queue id), then add keys — **generated** (`omq-…`) or **any string you choose** (8–256 printable characters, unique across clients). A key is opened from its row to **edit** (label, log capture) or **delete**; a client can be renamed, disabled (its keys get `403`) or deleted. A key always belongs to the client it was created for — moving one means deleting it and handing out a new one.
- Each client row carries **VIP**, **boost**, **Block IP**, **+ Key** and **Disable**/**Enable**; each key row carries **Edit** and **Delete**.
- A request carrying a client key (`Authorization: Bearer <key>` or `X-Api-Key: <key>`) queues under that client's name, **whatever `X-User-ID` it sends**. The key is removed before the request is forwarded, so it never reaches a backend (the backend's own `token` applies instead).
- Requests with no key, or a key no client owns, are queued by `X-User-ID` — **except that registered names are reserved**: a keyless request claiming one (e.g. `X-User-ID: alice` without alice's key) is queued apart as `unverified:alice`, with none of alice's queue position, priority or stats.
- **Blocking a registered client disables it** (*Block* / *Disable* in the web UI): its keys get `403` and its queued requests are dropped. That is the client's one block switch, saved with it; a user block on a client name from before this rule is converted to *disabled* at startup. Unregistered ids and IPs are blocked as before (`blocked_items.json`).
- **VIP and boost on a registered client are saved** with it and restored at startup; on an unregistered id they last until restart.
- **Renaming a client moves its live state** — queued requests, counters, last IP, fair-share position, VIP/boost — to the new name. Requests already being served finish under the old name. Deleting a client turns its name back into an ordinary id and clears its VIP/boost.
- Keys are stored in plaintext in `settings.clients_file` (default `clients.yaml` next to `appconf.yaml`) so they can be looked up again; the file is written owner-only (`0600`). Changes are saved immediately (atomic replace). A clients file that exists but cannot be parsed stops the proxy at startup rather than being overwritten. The file is hand-editable YAML — a key may be written as just its string (`- omq-…`); ids and timestamps are filled in when the proxy saves it.

  ```yaml
  clients:
    - name: codex-laptop
      redact_log: true      # keep the log entries, store no body text (see Request logging)
      keys:
        - key: omq-7f3a9c41d2e8b6057a1c9e3f4b2d8a60
          label: laptop
        - key: codex-review-2026
          label: auto-review
          redact_log: true  # same switch per key; a key can only opt out, never back in
  ```

#### Model filtering

The **Models** page decides which models the proxy serves. Rules are case-insensitive globs (`*` any run of characters, `?` one character) matched against the model names backends list, for every backend or only some:

- **Hide** rules always win. **Allow** rules matter only in *Serve only allowed* mode; the default mode, *Serve all except hidden*, serves every model no hide rule names. Switching modes is safe: rules of the other kind are kept and come back when you switch back.
- A model's switch adds or removes an exact-name rule. A model still hidden by a pattern rule cannot be switched on from the list — remove or narrow the pattern.
- Hidden models are left out of the merged `/v1/models` and `/api/tags`, and a request naming one (in the body, or the path of `/v1/models/{id}`) gets **`404 model not found`** at once, in OpenAI or Ollama error shape. Matching follows routing, so `llama3` cannot reach a hidden `llama3:latest`.
- A rule limited to some backends hides the model there only; requests route to the backends that still serve it.
- Model load/unload from the web UI still works on hidden models (to warm one before serving it). `/api/ps` is not filtered.
- Rules are stored in `settings.models_file` (default `models.yaml`), written atomically. A file that exists but cannot be parsed stops the proxy at startup rather than serving everything. It is hand-editable YAML — a plain hide rule may be written as just its pattern (`- unsloth/Qwen3.8-27B-GGUF:*`).

### 📈 Prometheus Metrics

`GET /metrics` serves Prometheus text format. Like `/health` it is open by default; set `settings.metrics_token` (or `OLLAMA_MQ_METRICS_TOKEN`) to require `Authorization: Bearer <token>`.

```yaml
scrape_configs:
  - job_name: ollamamq
    scrape_interval: 15s
    static_configs:
      - targets: ["ollamamq-host:11435"]
    # authorization: { credentials: "<token>" }   # when metrics_token is set
```

**Labels:** `client` (registered client name, unregistered `X-User-ID`/`ip-…`, or `unverified:<name>`), `model` (as requested; `unknown` for names no backend lists, `none` for model-less requests), `backend` (`host:port`), `endpoint` (route template). `client` and `model` keep at most 200 distinct values each; beyond that they read `other`.

| Metric | Type | Labels |
|---|---|---|
| `ollamamq_requests_total` | counter | client, model, endpoint, outcome (`ok` `client_gone` `backend_error` `timeout` `blocked` `hidden_model` `queue_full` `overloaded`) |
| `ollamamq_backend_responses_total` | counter | client, model, backend, code |
| `ollamamq_tokens_total` | counter | client, model, backend, kind (`prompt` / `completion`) |
| `ollamamq_request_bytes_total` / `ollamamq_response_bytes_total` | counter | client, model (, backend) |
| `ollamamq_client_key_requests_total` | counter | client, key_id, key_label |
| `ollamamq_client_key_tokens_total` | counter | client, key (label, else id), kind |
| `ollamamq_queue_wait_seconds` | histogram | client, model |
| `ollamamq_time_to_first_byte_seconds` / `ollamamq_request_duration_seconds` | histogram | model, backend |
| `ollamamq_time_to_first_token_seconds` | histogram | client, model, backend — dispatch to the first generated token (streams), or the backend's prompt time (non-streamed responses that report it) |
| `ollamamq_generation_seconds_total` / `ollamamq_generation_tokens_total` | counter | client, model, backend — decode time and the tokens generated in it |
| `ollamamq_prompt_seconds_total` / `ollamamq_prompt_eval_tokens_total` | counter | client, model, backend — prefill time and the prompt tokens evaluated (cache hits excluded) |
| `ollamamq_affinity_decisions_total` | counter | result — `hit` (a conversation stayed on its backend) / `moved` (it went elsewhere) |
| `ollamamq_cache_scheduling_total` | counter | action — `held_back` (a KV-cache-cold conversation gave its turn to a cache-hot one) or `reserved_first` (a session's follow-up went ahead of other waiting users inside its slot reservation) |
| `ollamamq_generation_speed_tokens_per_second` | histogram | model, backend — one observation per finished request |
| `ollamamq_queue_depth`, `ollamamq_queue_oldest_seconds` | gauge | client, model — waiting requests and the age of the oldest |
| `ollamamq_requests_in_flight`, `ollamamq_client_fair_share_score` | gauge | client |
| `ollamamq_active_requests`, `ollamamq_active_oldest_seconds`, `ollamamq_active_generated_tokens`, `ollamamq_active_prefilling`, `ollamamq_active_generation_seconds` | gauge | client, key, model, backend — requests in progress right now (live speed = generated tokens / generation seconds) |
| `ollamamq_backend_free_slots` | gauge | backend — requests it could start now (0 while offline or loading a model) |
| `ollamamq_backend_max_concurrency` | gauge | backend, source — its capacity; `source` is `detected` (slots the backend reports) or `config` |
| `ollamamq_model_max_concurrency` | gauge | model — `models[].max_concurrent_requests` |
| `ollamamq_client_info` | gauge (=1) | client, registered, disabled, vip, boost |
| `ollamamq_backend_up`, `ollamamq_backend_requests_in_flight`, `ollamamq_backend_max_concurrency`, `ollamamq_backend_models`, `ollamamq_model_loaded`, `ollamamq_backend_model_requests_in_flight` | gauge | backend (, api_type / state / model) |
| `ollamamq_model_control_operations_total` | counter | backend, model, action, result |
| `ollamamq_clients`, `ollamamq_blocked`, `ollamamq_model_policy_rules`, `ollamamq_model_policy_mode`, `ollamamq_queued_bytes`, `ollamamq_request_log_dropped_total`, `ollamamq_build_info`, `ollamamq_start_time_seconds` | various | — |

**Tokens** are counted live while a response streams (each chunk carrying generated text counts one, so runs in progress show up in `tokens_total` and the `active_*` gauges), then topped up to the exact figure when the response ends. The exact figure is read from the end of each response: OpenAI/llama.cpp `usage`, Ollama `prompt_eval_count`/`eval_count`, and for llama.cpp streams (which send no `usage` unless asked) the final chunk's `timings` (`cache_n + prompt_n` prompt, `predicted_n` completion). Other OpenAI-style streams only carry usage when the client asks for it (`"stream_options": {"include_usage": true}`); without it nothing is counted for that request.

**Speed** uses the backend's own timings when it reports them (llama.cpp `timings.prompt_ms` / `predicted_ms`, Ollama `prompt_eval_duration` + `load_duration` / `eval_duration`). Otherwise generation time is measured from the first streamed token to the end of the response. Per-stream decode speed is `rate(generation_tokens_total) / rate(generation_seconds_total)`, the speed one user sees; `rate(tokens_total{kind="completion"})` is a backend's total throughput across parallel streams.

Useful queries:

```promql
sum by (client) (rate(ollamamq_requests_total{outcome="ok"}[5m]))          # requests/s per client
sum by (model) (rate(ollamamq_tokens_total{kind="completion"}[5m]))         # generated tokens/s per model
sum by (client, model) (increase(ollamamq_tokens_total[24h]))               # daily usage matrix
histogram_quantile(0.95, sum by (le, client) (rate(ollamamq_queue_wait_seconds_bucket[5m])))
sum by (outcome) (rate(ollamamq_requests_total{outcome!="ok"}[5m]))         # failures by kind
sum by (backend) (rate(ollamamq_generation_tokens_total[5m])) / sum by (backend) (rate(ollamamq_generation_seconds_total[5m]))  # tok/s per stream
histogram_quantile(0.95, sum by (le, backend) (rate(ollamamq_time_to_first_token_seconds_bucket[5m])))   # TTFT p95
```

### 🧲 Sticky Routing (Session Affinity)

Without it, every request independently picks the least-loaded eligible backend, so the
second turn of a conversation often lands somewhere that has no KV cache for its prompt
and re-prefills the whole context. With it, the proxy remembers which backend served a
conversation's previous turn and prefers that one.

**It is a preference, never a pin.** If the remembered backend is offline, no longer serves
the model, or is running more than `settings.sticky_lag` more requests than the emptiest
eligible backend, the request goes through normal load-balancing instead and the
conversation follows it — moving costs one re-prefill, while pinning to a busy backend
would stall everything else in that user's queue.

What counts as "the same conversation", by `settings.sticky_routing`:

| value | key | use |
|---|---|---|
| `off` | — | today's behavior |
| `header` | `X-Session-Id` header, or the `prompt_cache_key` field OpenAI/Ollama clients send | exact, when you control the client |
| `prompt` *(default)* | plus a fingerprint of the conversation's **first turn** | chat clients resend the whole history every turn, so untouched clients get affinity for free. The opening turn is the one part that never changes — hashing the whole history (or its tail) would make every turn look like a new conversation. Whitespace, field order and a leading `system` turn do not change it, and at most the first 16 KiB of it is hashed. Two threads that open with the same words share one preference, which costs nothing but a shared backend while both are idle. |
| `identity` | plus the client id itself | one home backend per client — right for one session per user, wrong for a client running several conversations at once |

A different model is a different conversation (different cache). Only completion routes are
keyed (`/api/chat`, `/api/generate`, `/v1/chat/completions`, `/v1/completions`), and the
fingerprint needs the request body — so TLS must terminate at ollamaMQ, not in front of it.

Remembered conversations expire after `settings.sticky_ttl` idle seconds (raise it towards
`load_keep_alive` when contexts stay resident for hours), are capped at
`settings.sticky_max_entries`, and are all forgotten when the backend list changes.

**Tuning and checking it.** `ollamamq_affinity_decisions_total{result="hit|moved"}` counts
the outcomes; the payoff shows up as a drop in
`rate(ollamamq_prompt_eval_tokens_total[5m])` per client once conversations are warm. If
`moved` dominates while backends are idle, raise `sticky_lag`. If prompt-eval stays high even
when conversations are hitting their own backend, the problem is not routing: the backend
cannot hold that many contexts in its cache at once (`OLLAMA_NUM_PARALLEL`, `num_ctx`,
`load_keep_alive` — or more VRAM), and no routing choice fixes that.

### 🏷️ Session Labels & Priority

Every conversation the proxy can identify has a six-hex **session tag** (the sticky-routing
session id, shown as `#a3f19c`). Two headers ride along with it:

| header | meaning |
|---|---|
| `X-Session-Id` | the conversation id — also what sticky routing keys on |
| `X-Session-Name` | a readable label for it, shown as `#refactor-auth` |
| `X-Session-Priority` | `high` or `top`: how far this conversation jumps the queue |

The **label is display text only**. It never enters the session key, so renaming a session
mid-conversation cannot re-key it and throw away its backend affinity (the same trap as a
compaction rewriting the history). It is kept to 64 characters of letters, digits, `-` and
`.`, arrives lowercased with separators collapsed and control characters stripped, and is
shown in Activity, in the request log and on the run rows. A conversation with no label is
shown by its tag, as before.

Three levels, no numeric weights:

| level | what it does |
|---|---|
| `normal` | the fair order everything else queues in |
| `high` | first **inside its own client's queue** — no other client moves, so one busy user cannot starve another |
| `top` | first on the whole board: considered before the VIP client, never held back by the cache hold-back (and not charged against its yield budget), and allowed **one** extra dispatch on the backend its conversation is warm on when that backend is otherwise full — never a second |

Nothing is ever cancelled to make room: a `top` request wins the next slot, it does not take
one away from a request already running.

How far a client's own claim reaches, by `settings.client_priority`:

| value | a client may claim |
|---|---|
| `off` | nothing — priority is the operator's alone |
| `self` | `high`; a `top` claim is clamped to `high` |
| `global` *(default)* | `high` and `top` |

Two limits hold in every mode: the header counts only for **authenticated** requests (an API
key) — a keyless `ip_identity` request never gets a say in the order everyone else waits in —
and an operator pin always beats the client's claim, in both directions, including holding a
session down at `normal`. `global` means what it says: any client holding an API key can put
itself first on the shared board, so on a proxy shared between tenants `self` — or `off`, and
pin the sessions you actually care about — is the safer setting.

The operator sets it from three places, never from `appconf.yaml` (a pin is live state, not
configuration):

- **Web** — the icon beside a session *is* the control: clicking it cycles
  normal → high → top → normal (`▲` violet = top, `△` berry = high, grey `○` =
  nothing yet — every live conversation carries one in its Details header, on
  any screen width). The status line counts the conversations sitting above
  normal, and the Filters modal can show only those.
- **API** — `POST /ui/api/sessions/<tag>/priority` with `{"level":"top"}` (`high` and
  `normal` too), same auth as everything under `/ui/api/`. `400` if the tag is not six hex
  digits or the level is not one of the three.

A pin is keyed by the **tag** rather than by the conversation behind it, so it can be set the
moment a session is visible — including while its very first request is still running, when
there is nothing yet to look the conversation up in. It takes effect on that conversation's
**next** turn (what is already running keeps the level it was queued with; nothing is taken
away mid-flight). The icon beside the session shows the pin, so the click visibly sticks even
on a finished row. Pins live in memory and are dropped once nothing carries the tag for
`sticky_ttl`, so a restart returns everything to `normal`.

### Cache-Aware Hold-Back

When a backend's KV cache cannot hold every active conversation, plain per-request
round-robin is the worst ordering for it: every turn hands the GPU to a conversation whose
context the previous turn just evicted, and each switch is billed as a full re-prefill.
Where the backend exposes the two signals — llama.cpp/halogen do (`kv_cache_usage` with
`--metrics`, per-request `prompt_n` timings) — ollamaMQ reads them: a conversation whose
last prefill on a backend hit below 50%, and whose estimated next-turn increment (what the
backend evaluated and decoded last turn) does not fit the free KV the backend reports,
queues behind the conversations that backend served most recently (the two whose contexts
the cache still holds), so the resident pair keeps hitting instead of trading misses one
request at a time. A held request yields at most 16 such turns — a per-request budget, not
a queue-time bound, because in a saturated queue every request has already "waited too
long" — and anything unmeasured (new conversation, backend without telemetry, backend that
cannot size its cache) falls back to the coarser signals or the plain fair order, so an
uninstrumented fleet schedules exactly as before.

On top of that, a session that finished a turn keeps queue priority for its own follow-up:
the turnaround between an answer and the next request is the client running its tool
(median 0.5 s, max 4.3 s in the reference logs), so the window adapts per session — 1.5x
its own worst recent pause, clamped 2–30 s — and expires exactly when the session has gone
quiet, letting a whole agent cycle run on one warm cache. A session wins at most 64
reserved dispatches in a row before it stands one ordinary fair round, so a never-quiet
agent cannot monopolise the queue.

`ollamamq_cache_scheduling_total` counts both mechanisms: `held_back` = a cold
conversation gave way to a hot one, `reserved_first` = a session's follow-up went ahead of
other waiting users; `kv % free` on the backend card is the pressure signal itself.
Replays of a saturated Brainiac hour: see `tests/sim/`.

### Port Configuration

- **11435**: The proxy port that clients connect to (exposed by default)
- **11434**: The Ollama server port (internal, not exposed)

To change the proxy port, use the `PORT` environment variable:

```bash
docker run -d \
  --name ollamamq \
  -p 8080:8080 \
  -e PORT=8080 \
  ghcr.io/dmarchevsky/ollamamq
```

## 🧪 Development

```bash
cargo test                    # unit tests: routing, sessions, queueing, the log
bash tests/e2e/run.sh         # model load/unload and the admin gate, against mock backends
```

## 📝 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details (if applicable).
