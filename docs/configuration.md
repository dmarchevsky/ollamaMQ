# Configuration

[← Back to README](../README.md)

Everything about an ollamaMQ instance lives in one YAML file, `appconf.yaml`, plus a few environment variables and command-line flags.

**Precedence:** command-line flags and environment variables > `appconf.yaml` > built-in defaults.

The web UI's **Settings** and **Backends** pages edit this same file while the proxy runs — see [Web UI → Settings](web-ui.md#settings). A fully commented template is in [`appconf.yaml.example`](../appconf.yaml.example).

## appconf.yaml

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
  request_log_path: ollamamq-requests.jsonl   # conversation JSONL log file (default shown; a relative path resolves from the appconf.yaml folder)
  request_log_max_total_bytes: 1073741824    # bytes on disk the conversation log may take (default 1 GiB)
  request_log_max_days: 30                    # days a conversation is kept after its last turn (default 30; 0 = off)
  request_log_flush_secs: 5                   # quiet time after which a conversation is written out (default 5)
  log_content_limit: 1048576                  # max body bytes captured per request, min 1024 (default 1 MiB)
  history_hours: 24                # how far the Activity (Overview) history reaches, and how long its
                                     # journaled records survive a restart (default 24; journal: ollamamq-activity.jsonl
                                     # next to the request log — size it for retention the same way)
  history_max_records: 50000       # finished requests kept in memory for the Overview, whatever their age (default)
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

## Command-line flags

Flags win over `appconf.yaml` when given.

| Flag | Overrides | Meaning |
| --- | --- | --- |
| `-c, --model-config <path>` | — | the configuration file (default `appconf.yaml`) |
| `-b, --backend-urls <a,b,…>` | `backends` | comma-separated backend URLs (`--ollama-urls` is an alias); when given, the Backends page is read-only |
| `-p, --port <n>` | `settings.port` | listen port (default 11435) |
| `-H, --host <addr>` | `settings.host` | bind interface (default `127.0.0.1`) |
| `-t, --timeout <secs>` | `settings.timeout` | per-request timeout (default 300) |
| `--load-keep-alive <secs>` | `settings.load_keep_alive` | how long model-control loads stay resident; `-1` = forever |
| `--allow-all-routes` | `settings.allow_all_routes` | also proxy non-standard endpoints |
| `--no-tui` | — | deprecated and ignored (the terminal dashboard is gone); logs one warning |

## Environment variables

Read by the binary itself (anywhere — shell, systemd, Docker):

| Variable | Description | Default |
| --- | --- | --- |
| `OLLAMA_MQ_API_KEY` | Proxy API key; enables auth when set — see [API → Authentication](api.md#authentication) | *(unset — auth disabled)* |
| `OLLAMA_MQ_UI_USER` | Web UI sign-in user (overrides `settings.web_ui.username`) | `admin` |
| `OLLAMA_MQ_UI_PASSWORD` | Web UI sign-in password (overrides `settings.web_ui.password`); enables the web UI. A `settings.web_ui.password_hash` wins over it | *(unset — web UI off)* |
| `OLLAMA_MQ_METRICS_TOKEN` | Bearer token required by `GET /metrics` (overrides `settings.metrics_token`) | *(unset — open)* |
| `RUST_LOG` | Log level — see [Deployment → Logging](deployment.md#logging) | `debug` |

Read only by the Docker image's entrypoint script, which turns them into command-line flags (the shipped `docker-compose.yml` bypasses the entrypoint and uses `appconf.yaml` instead):

| Variable | Description | Default |
| --- | --- | --- |
| `BACKEND_URLS` | Comma-separated backend URLs (`--backend-urls`) | `http://localhost:11434` |
| `OLLAMA_URLS` | Legacy alias for `BACKEND_URLS` (used only when it is unset) | — |
| `PORT` | Listen port (`--port`) | `11435` |
| `HOST` | Bind interface (`--host`) | `0.0.0.0` |
| `TIMEOUT` | Request timeout in seconds (`--timeout`) | `300` |

## Model aliases and Olla-style paths

`settings.model_aliases` lets clients ask for a model under another name: a request whose `model` matches an alias (any case) is rewritten to the target before routing, so the model policy, logs and metrics all see the real name. Olla-style URLs (`/olla/openai/v1/...`, `/olla/proxy/api/...`) are accepted too and served as the plain routes, so a client set up for Olla only needs a new port. Only known routes are served behind those prefixes.

## Decision-model backends (System One)

A third backend family: "decision model" servers — `clm-serve` ([CLM](https://github.com/Contrastive-LM/CLM)), `laya-serve` ([Laya](https://github.com/NandhaKishorM/laya)), `kev.serve` ([Kev](https://github.com/jaredpalmer/kev)) — which answer `POST /v1/systemone` with typed decisions (`choice` / `score` / `noul`) instead of text. The body is passed through byte-for-byte (never parsed), and it goes **only** to backends of this family: these servers implement **no `/api/*` route at all**, so `/api/chat`, `/api/tags` and `/v1/chat/completions` are never sent to them, and they never answer a route their family cannot serve — such a request is refused, not misrouted. Only `/v1/systemone` is proxied; `/v1/rank` and Kev's `/v1/systemone/permute|separate` are not.

- **Detection** — a backend answering `GET /v1/models` with `{"models":[{"name":…}]}` (CLM, Kev) is identified as System One; a 2xx `GET /health` marks a backend online when nothing else identifies it. Laya exposes no listing endpoint and therefore cannot be positively detected — pin it with `kind: systemone` on the backend entry. Auth on all three is `Authorization: Bearer <key>`, which is exactly what the existing `token:` key already sends (`CLM_API_KEY` / `LAYA_API_KEY` / `KEV_API_KEY`).
- **Model names** — CLM and Kev are listed by the probe automatically. A Laya server has nothing to list, so its checkpoints must be declared under `models:`: there, a `models:` entry makes a name **routable**, not merely pinned, and a `models:` entry with no `backends:` selector is admitted on *every* backend — use an explicit selector as soon as you run more than one backend.
- **Listings** — decision-model names appear in the aggregated `GET /v1/models` **and** `GET /api/tags` even though only `/v1/systemone` can serve them. That is deliberate: ollamaMQ aggregates one namespace and routes by capability. `/v1/models` always answers in OpenAI's `data`/`id`, `/api/tags` always in Ollama's `models`/`name`; names for a backend that cannot list anything are synthesised from proxy state and appended after the live entries, so real metadata (`size`, `digest`, `description`) always wins.
- **Model control is not available** for this family — see [Model Control](api.md#model-control-admin-api).
- ⚠️ **`laya-serve` binds `0.0.0.0:8000` and `clm-serve` binds `0.0.0.0:8700` open by default** — anyone on the LAN can run inference against them. Bind them to loopback or set their API key. `kev.serve` binds `127.0.0.1:8008`.
- `clm-serve` needs a separate vLLM pooling encoder (default `http://127.0.0.1:8090/v1/embeddings`) and answers `502` while that is down. That is CLM replying, not ollamaMQ declaring the backend dead.

```bash
curl -X POST http://localhost:11435/v1/systemone \
  -H "X-User-ID: developer-1" \
  -d '{"model": "clm-latest", "state": "billed twice, refund please or we cancel",
       "questions": {"refund": {"type": "noul", "instructions": "Is a refund justified?"}}}'
```

Related: [scheduling and routing](scheduling.md), [model filtering](web-ui.md#model-filtering), [clients and API keys](web-ui.md#clients-and-api-keys).
