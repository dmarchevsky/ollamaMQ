# ollamaMQ

`ollamaMQ` is a high-performance, asynchronous message queue dispatcher and load balancer designed to sit in front of one or more [Ollama](https://ollama.ai/) or [LM Studio](https://lmstudio.ai/) API instances. It acts as a smart proxy that queues incoming requests from multiple users and dispatches them in parallel to multiple backends using a fair-share round-robin scheduler with least-connections load balancing.

![Rust](https://img.shields.io/badge/rust-2024-orange.svg)
![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Ollama](https://img.shields.io/badge/Ollama-Proxy-7ed321.svg)

## 🚀 Features

- **Multi-Backend Load Balancing**: Distribute requests across multiple Ollama or LM Studio instances using a **Least Connections + Round Robin** strategy. Automatically detects backend API type (Ollama `/api/*` vs OpenAI `/v1/*`) and routes each request only to a compatible backend — even when a specific model is requested, so an Ollama-family call never reaches an LM Studio server that merely lists the same model.
- **Model-Aware Routing**: Automatically identifies the requested model from the request body and routes it only to backends that list that model. Among those, selection is load-aware: first to backends where the model is already loaded and ready (Ollama `/api/ps`, LM Studio loaded instances; vLLM-style backends that don't report loaded state count as ready when they list the model), then to cold backends with nothing loaded, and only as a last resort to a backend that would have to evict a different loaded model — so a request for `qwen3.8-27b` goes to the server that has it in GPU memory instead of making a busy server with another model load it. Name matching is deterministic and bounded — exact, `:tag`/case-insensitive, or publisher-/quant-suffixed variants of the same id (`qwen3.8-27b` reaches `unsloth/qwen3.8-27b@q8_0`) — but never arbitrary substrings (so it will not be routed to an unrelated `...-abliterated` build).
- **Smart Model Matching**: Robust matching that handles common variations like `:latest` tags and case-insensitivity. For example, a request for `llama3` will correctly match `llama3:latest` on the backend.
- **Model Control**: Load and unload models on connected backends (Ollama and LM Studio 0.3.6+) directly from the TUI (`L`/`U`) or the admin HTTP API — without touching the backend servers themselves.
- **Parallel Processing**: Unlike basic proxies, `ollamaMQ` can process multiple requests simultaneously (one per available backend), significantly increasing throughput for multiple users.
- **Aggregated Model Listings**: `GET /api/tags` and `GET /v1/models` are answered by the proxy from **every** compatible backend at once, not routed to one of them. Each backend's own listing is merged (so per-model metadata like `size`, `digest` and `details` is preserved exactly as that backend reported it) and deduplicated by name/id. Clients therefore see everything the proxy can route to, and the list no longer changes from one poll to the next depending on which backend happened to answer.
- **Backend Health Checks**: Automatically monitors backend status every 10 seconds. All backends are probed **concurrently**, each request bounded by its own 5-second timeout, so one unresponsive backend can't hold up everyone else's status. Probes cover both API type (Ollama vs OpenAI) and the list of currently available models (via `/api/tags` and `/v1/models`). Endpoints a backend rejects are remembered and skipped until re-checked, so incompatible backends don't spam warnings. Offline instances are temporarily skipped and marked in the TUI.
- **Fail-Fast for Unsatisfiable Requests**: If no online backend can ever serve a queued request (wrong API family or model absent everywhere), it is answered with `503` after `stuck_timeout` seconds instead of hanging forever. Requests merely waiting for a busy or loading backend are unaffected.
- **Per-Model Concurrency Limits**: Each model entry's `max_concurrent_requests` caps how many in-flight requests one backend may serve for that model, bounded globally by `settings.max_concurrent_per_backend` (default 1 — the historical one-request-per-backend behavior; raise it to let a single backend handle several requests at once). The limit is keyed on the model's base name, so it applies no matter how the client spells it — `qwen3.8-27b`, `qwen/qwen3.8-27b` and `unsloth/qwen3.8-27b@q8_0` all draw on the same budget as the configured entry.
- **Metadata Reads Never Queue**: `/`, `/api/tags`, `/api/ps`, `/api/version`, `/api/show`, `/v1/models` and `/v1/models/{model}` don't run inference, so they neither wait for a free backend slot nor occupy one — a chat UI polling `/api/tags` on a timer is answered immediately instead of queueing behind a multi-minute generation (or a model load) with the default `max_concurrent_per_backend: 1`.
- **Per-User Queuing**: Each user (identified by a client API key, else by the `X-User-ID` header, else by address as `ip-<address>`) has their own FIFO queue, capped at 100 waiting requests — beyond that the proxy answers `429` rather than growing the queue.
- **Bounded Memory**: A queued request holds its whole body until a backend takes it, so bodies are capped two ways: `settings.max_body_bytes` per request (default 64 MiB, `413` beyond it) and `settings.max_queued_bytes` across all queues at once (default 512 MiB, `503` beyond it). A request is always admitted when the queues are empty, so an oversized body can never lock the proxy out of making progress. `/api/blobs/{digest}` keeps a separate 1 GiB allowance for model-layer uploads.
- **Fair-Share Scheduling**: Prevents any single user from monopolizing all available backends. A user's turn is decided by their **recent** load: every dispatched request adds to a score that halves every 5 minutes, so a long-idle user isn't punished for past traffic and a newcomer can't monopolize the proxy while "catching up". Requests are charged when dispatched regardless of outcome, so a client whose requests always fail or disconnect can't keep jumping the queue.
- **Sticky Routing (Session Affinity)**: A conversation goes back to the backend that served its previous turn, so a long multi-turn prompt is not re-prefilled on a backend with no KV slot for it. The key is an explicit session id when the client sends one, else a fingerprint of the conversation's first turn (see [Sticky Routing](#-sticky-routing-session-affinity)).
- **Transparent Header Forwarding**: Full support for all HTTP headers (including `X-User-ID`) passed to and from the backend, ensuring compatibility with tools like **Claude Code**.
- **VIP & Boost Modes**: Absolute priority (VIP) or increased frequency (Boost) for specific users.
- **Real-Time TUI Dashboard**: Monitor backend health, active requests, queue depths, and throughput in real-time.
- **Prometheus Metrics**: `GET /metrics` reports requests, outcomes, tokens, bytes and latency **per client and per model**, plus live queue and backend state.
- **Web UI**: Everything the TUI does, in a browser at `/ui` behind a sign-in page — plus management of clients and their API keys.
- **Clients & API Keys**: Give each client one or more keys (generated or any string you choose). A request with a client key queues under the client's name, so a key holder can't hop queues by changing `X-User-ID`.
- **OpenAI Compatibility**: Supports standard OpenAI-compatible endpoints.
- **Async Architecture**: Built on `tokio` and `axum` for high concurrency.

![ollamaMQ TUI Dashboard](demo.gif)

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

All three endpoints are served by the proxy itself (they are never proxied to a backend) and are protected by the same optional `OLLAMA_MQ_API_KEY` auth as the proxy routes.

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
- `identifier` — free-form label shown with the operation in the TUI and API responses.

Responses:

| Status | Meaning |
| ------ | ------- |
| `202`  | Accepted — the operation started in the background (response body contains the canonical model name and the backend URL). |
| `400`  | Bad request — empty model, backend offline, unsupported backend type, or unloading a model that is not loaded. |
| `404`  | Unknown backend, model not found on the backend, or no suitable backend for `"any"`. |
| `409`  | Conflict — another control operation is already in flight on that backend, or the backend is busy with active requests. |

```bash
# Load (llama3 resolves to llama3:latest)
curl -s -X POST http://localhost:11435/admin/models/load \
  -H "Content-Type: application/json" \
  -d '{"backend": 0, "model": "llama3"}'

# Unload by URL (frees GPU memory on Ollama)
curl -s -X POST http://localhost:11435/admin/models/unload \
  -H "Content-Type: application/json" \
  -d '{"backend": "http://10.0.0.2:11434", "model": "qwen2.5:7b"}'

# Let the proxy pick a backend
curl -s -X POST http://localhost:11435/admin/models/load \
  -H "Content-Type: application/json" \
  -d '{"backend": "any", "model": "llama3:latest"}'

# With API-key auth enabled
curl -s http://localhost:11435/admin/models -H "Authorization: Bearer supersecret"
```

**Note:** Ollama keeps a model loaded for `keep_alive` seconds only (its own default is 5 minutes). A control "load" sends the `--load-keep-alive` value (default 24 h) so the model actually stays resident; an "unload" sends `keep_alive: 0`.

### Logging

`ollamaMQ` uses [`tracing`](https://docs.rs/tracing) and behaves differently depending on the mode:

- **TUI mode** (default, interactive terminal): Logs are written to the `ollamamq.log` file in the current working directory. This keeps the terminal clear for the dashboard. Default level is `info`.
- **`--no-tui` mode** (Docker/CI/service): Logs are written to **stderr** at a default level of `debug`, so you can see everything — backend health checks, per-request routing, backend detection, and errors. This is ideal for capturing output via the systemd journal, Docker, or piping to a file.

Override the level at any time with the standard `RUST_LOG` environment variable:

```bash
# Most verbose (all debug detail)
RUST_LOG=debug ollamaMQ --no-tui

# Quieter (info + errors only)
RUST_LOG=info ollamaMQ --no-tui
```

### Running as a systemd Service

To run `ollamaMQ` as a background service with full log visibility in the journal:

1. Copy the provided service file to your system's unit directory:

   ```bash
   sudo cp ollamamq.service /etc/systemd/system/
   ```

2. Edit the file and set the correct `ExecStart` path to your installed binary (e.g. `~/.cargo/bin/ollamaMQ`) and your backend URLs:

   ```ini
   ExecStart=/usr/local/bin/ollamaMQ --no-tui --port 11435 --backend-urls http://localhost:11434
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

Because `--no-tui` defaults to `debug` logging on stderr, **all** dispatcher events (backend health, request routing, errors) are captured in the journal — no separate log file needed.

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
  detect_backend_slots: true     # use the parallel slots a backend reports (llama.cpp /props total_slots,
                                 # halogen /health slots) as its capacity instead of max_concurrent_per_backend;
                                 # models without max_concurrent_requests may then use all of them (default true)
  max_queued_bytes: 536870912    # total bytes of request bodies waiting in all queues, 503 beyond it
                                 # (default 512 MiB); a request is always admitted when the queues are empty
  request_log_path: ollamamq-requests.jsonl   # request/response JSONL log file (default shown)
  request_log_max_bytes: 10485760             # rotate when the file exceeds this (default 10 MiB)
  request_log_max_files: 5                    # rotated files kept as .0 .1 ... (default 5)
  log_content_limit: 65536                    # max body bytes captured per record, min 1024 (default 64 KiB)
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
    identifier: "my-gpt"        # label attached to this entry's load op (TUI / admin API)
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
- `models` is applied automatically at startup (once backend probes have run) and re-applied any time you press **`r`** in the TUI. Application is additive: each target endpoint is checked live first (Ollama `/api/ps`, LM Studio loaded instances), so models already resident there — e.g. still loaded from an earlier run because of long `keep_alive` — are skipped instead of being loaded twice; everything else gets a load started. It never unloads anything, and loads for the same backend run one at a time (backends reject parallel control ops). Every attempt, including skips, is reported in the TUI Logs panel (`⟳ CTL`) and in the log output.
- A backend won't accept a load while it is serving requests, so each entry **waits for that backend to go idle** (up to 5 minutes) before loading. Without this, restarting the proxy under live traffic meant every configured model was refused on the spot and never retried — leaving models unloaded, or loaded with the wrong `max_ctx`.
- Explicit CLI flags override file values when given (e.g. `--backend-urls` over `backends`, `--port`/`--host`/`--timeout` over `settings`).
- A non-empty `models[].backends` list also **pins routing**: requests for that model are only sent to the listed backends (case-insensitive substring URL match, the same rule as load targeting) and never to other suitable ones. If none of the pinned backends is online/eligible, the request fails with HTTP 503 after `stuck_timeout` (error: `no configured backend available for model '<model>'`) instead of being routed elsewhere. If `backends` is empty/omitted, or the model is not in the config at all, the existing behavior applies — any online backend that lists the model may serve it.
- A `models:` entry does two things: it **loads** the model where the backend supports control, and it makes the name **routable** on the selected backends even where nothing can list it. The second half is the only way to reach a decision-model server with no listing endpoint (Laya), and it applies to backends without model control as well — for them the entry names and pins, it does not load.

See [`appconf.yaml.example`](appconf.yaml.example) for a fully commented template.

## Request logging

Every proxied request (`IN`) and upstream response (`OUT`) is written as JSON lines to `settings.request_log_path` (default `ollamamq-requests.jsonl`). Each line is one JSON object:

| field | meaning |
| --- | --- |
| `ts` | unix time in milliseconds |
| `dir` | `IN` (proxied request) or `OUT` (upstream response) |
| `user` | client address |
| `model` | model name, when resolvable (otherwise null) |
| `backend` | upstream backend URL (may be null; `<all backends>` for aggregated model listings) |
| `method` | HTTP method |
| `path` | request path |
| `status` | response status code (responses only) |
| `bytes` | total body size in bytes |
| `content_type` | body content type |
| `content` | body preview, truncated to `log_content_limit` with a `...[truncated: N bytes total]` marker |

**Rotation:** when the file exceeds `request_log_max_bytes` it is renamed to `.0` (existing `.0` → `.1`, etc.), at most `request_log_max_files` rotated files are kept and the oldest is deleted. The current file size is re-read at startup, so sizing/rotation survives restarts.

**TUI:** the bottom "Requests" panel shows these newest-first with a one-line content preview; press **Enter** on a row for the full-content detail view (`j`/`k` or `PgUp`/`PgDn` scroll, `q`/`Esc`/`Enter` close).

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

**Where store files go:** relative paths for `clients_file` and `models_file` are resolved from the folder that holds `appconf.yaml`, so by default they sit next to it. Logs (`request_log_path`, and `ollamamq.log` when the TUI runs) stay relative to the working directory.

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

```bash
# Enable auth
OLLAMA_MQ_API_KEY=supersecret ollamaMQ --no-tui

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
OLLAMA_MQ_UI_PASSWORD=change-me ollamaMQ --no-tui     # user defaults to "admin"
```

The web UI is **off** unless a password is configured (`OLLAMA_MQ_UI_PASSWORD`, or `settings.web_ui.password` in `appconf.yaml`). Browsers sign in on `/ui/login`, which sets a session cookie (`HttpOnly`, `SameSite=Strict`, `Secure` when the proxy in front sends `X-Forwarded-Proto: https`) valid for 7 days; **Sign out** is in the sidebar. Sessions are kept in memory, so restarting ollamaMQ signs everyone out. After 10 failed sign-ins within 10 minutes further attempts get `429` until the window passes. Scripts can skip the page and send the same user and password as HTTP basic auth to `/ui/api/*`. This is all independent of `OLLAMA_MQ_API_KEY`. The password crosses the network at sign-in, so put the UI behind TLS (a reverse proxy) when it is reachable from beyond a trusted network.

It covers everything the TUI does:

- **Overview** — status at a glance over the last 5 min / 15 min / 1 hour (see below)
- **Backends** — health, API type, load, and per-backend model lists with **Load** / **Unload** (TUI `L` / `U`); also where backends are configured: **Add** (with **Test connection**), name, token, **Disable/Enable**, **Remove**
- **Clients** — every queue identity with its queue depth and counters; **VIP**, **Boost**, **Block**, **Block IP**, **Unblock** (TUI `p` `b` `x` `X` `u`)
- **Models** — every model the backends list, with a switch per model and filter rules (below)
- **Blocked** — disabled clients, blocked ids and blocked IPs
- **Logs** — recent requests with a filter; click a row for the full captured content
- **Settings** — limits, model aliases, metrics token, the admin password, and the version you are running (see below)
- **Reload config** — re-read `appconf.yaml` (TUI `r`)

The **Overview** page shows:
- a one-line health summary
- a **Capacity** board: one proportional bar per backend — solid client-coloured segments for the running requests (width ∝ the request's estimated size) and clearly visible free slots; waiting requests get a slim row under the bar on the same token scale, so a backlog visibly passes the backend's end. Where a backend reports its KV cache (a llama.cpp server started with `--metrics`), the free space is sized by it; elsewhere free slots split the remainder equally
- a **client filter** row: one chip per client that appears in the window. Picking one scopes every number on the page — Queue, Running, Done, Capacity, the timeline and the lists — to that client (free slots and backend health stay backend-wide, they are not the client's). The choice is remembered per browser
- an **Activity** panel. The graph is one row per conversation, grouped under the backend it ran on: each request paints one block per stage — **waiting** (queued for a slot), **prefill**, **decode** — placed at the moment that stage began and as wide as it ran, so a 40s decode reads as forty seconds next to a 1s prefill. Colours mean the stage and nothing else; the client's own colour is the swatch beside its name. A conversation that goes quiet leaves a gap on its row, and a stage cut off by an edge of the window is clipped there and marked `‹`. Below the graph, **List** gives running and waiting requests (**All** adds the recently finished ones) with wait, prefill size and time, prefix-cache use, first token, generation time and speed; **Details** threads each conversation instead — one line per stage event with its own timestamp, plus an `idle` line where the conversation sat quiet. Sessions come from sticky routing: the client's `X-Session-Id` header, its `prompt_cache_key`, or a fingerprint of the conversation (see [Sticky Routing](#-sticky-routing-session-affinity))
- **Backends** and **Clients** tables with requests, errors, first-token p50/p95, generation and prefill speed, and token totals

Numbers cover the chosen window and come from an in-memory history of the last hour (finished requests, plus what is running or queued right now). The history starts empty on every restart. Request sizes are estimated as one token per four body bytes while queued or running — the exact prompt count only arrives with the finished response.
- *First token* is measured from dispatch to the first generated token for streams, or is the prompt time the backend reports for non-streamed responses.
- *Generation speed* is per stream (median).
- *Share of slot time* is each client's part of the backend time used in the window, which is what fair scheduling divides.

#### Backends

The **Backends** page lists the configured backends and edits the backend list while the proxy runs (saved into `appconf.yaml`, same rules as Settings):
- **Adding** a backend probes it within 10 s. **Test connection** shows its type, slots and models before you add it.
- **Disabling** one stops new requests at once; running ones finish.
- **Removing** one disables it now and drops it from the list at the next restart.
- **Names:** a backend can have a **name** (`backends: - url: …` with `name: gpu-box` in `appconf.yaml`, or in the UI). The UI shows the name instead of `host:port`. Prometheus keeps `backend="host:port"` and adds `ollamamq_backend_info{backend, name}` for dashboards.
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

It also manages **clients and their API keys**. The Clients page lists two kinds of client together (filter with *All / Registered / Unregistered*):

- **Registered clients** are created in the UI and saved in `clients.yaml`. They authenticate with their API keys and are listed even when idle.
- **Unregistered ids** are what requests without a key are queued under: their `X-User-ID`, or the IP a front proxy fills in, else `anonymous`. They live in memory and drop off the list after an hour idle. **Register** turns one into a client of the same name and opens the add-key dialog.

- Create a client (its name becomes its queue id), then add keys — **generated** (`omq-…`) or **any string you choose** (8–256 printable characters, unique across clients). A key can be relabelled, moved to another client, or revoked; a client can be renamed, disabled (its keys get `403`) or deleted.
- A request carrying a client key (`Authorization: Bearer <key>` or `X-Api-Key: <key>`) queues under that client's name, **whatever `X-User-ID` it sends**. The key is removed before the request is forwarded, so it never reaches a backend (the backend's own `token` applies instead).
- Requests with no key, or a key no client owns, are queued by `X-User-ID` — **except that registered names are reserved**: a keyless request claiming one (e.g. `X-User-ID: alice` without alice's key) is queued apart as `unverified:alice`, with none of alice's queue position, priority or stats.
- **Blocking a registered client disables it** (TUI `x`, web *Block* / *Disable*): its keys get `403` and its queued requests are dropped. That is the client's one block switch, saved with it; a user block on a client name from before this rule is converted to *disabled* at startup. Unregistered ids and IPs are blocked as before (`blocked_items.json`).
- **VIP and boost on a registered client are saved** with it and restored at startup; on an unregistered id they last until restart.
- **Renaming a client moves its live state** — queued requests, counters, last IP, fair-share position, VIP/boost — to the new name. Requests already being served finish under the old name. Deleting a client turns its name back into an ordinary id and clears its VIP/boost.
- Keys are stored in plaintext in `settings.clients_file` (default `clients.yaml` next to `appconf.yaml`) so they can be looked up again; the file is written owner-only (`0600`). Changes are saved immediately (atomic replace). A clients file that exists but cannot be parsed stops the proxy at startup rather than being overwritten. The file is hand-editable YAML — a key may be written as just its string (`- omq-…`); ids and timestamps are filled in when the proxy saves it.

#### Model filtering

The **Models** page decides which models the proxy serves. Rules are case-insensitive globs (`*` any run of characters, `?` one character) matched against the model names backends list, for every backend or only some:

- **Hide** rules always win. **Allow** rules matter only in *Serve only allowed* mode; the default mode, *Serve all except hidden*, serves every model no hide rule names. Switching modes is safe: rules of the other kind are kept and come back when you switch back.
- A model's switch adds or removes an exact-name rule. A model still hidden by a pattern rule cannot be switched on from the list — remove or narrow the pattern.
- Hidden models are left out of the merged `/v1/models` and `/api/tags`, and a request naming one (in the body, or the path of `/v1/models/{id}`) gets **`404 model not found`** at once, in OpenAI or Ollama error shape. Matching follows routing, so `llama3` cannot reach a hidden `llama3:latest`.
- A rule limited to some backends hides the model there only; requests route to the backends that still serve it.
- Model load/unload from the TUI or web UI still works on hidden models (to warm one before serving it). `/api/ps` is not filtered.
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

### Stress Testing

You can use the provided `test_dispatcher.sh` script to simulate multiple users and verify the dispatcher's behavior under load:

```bash
./test_dispatcher.sh
```

![ollamaMQ Stress Test](demo-test.gif)

## 📝 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details (if applicable).
