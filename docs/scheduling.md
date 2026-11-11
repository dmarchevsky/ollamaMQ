# Routing and scheduling

[← Back to README](../README.md)

How ollamaMQ decides which request runs next, and where.

## At a glance

- **Multi-Backend Load Balancing**: Distribute requests across multiple Ollama or LM Studio instances using a **Least Connections + Round Robin** strategy. Automatically detects backend API type (Ollama `/api/*` vs OpenAI `/v1/*`) and routes each request only to a compatible backend — even when a specific model is requested, so an Ollama-family call never reaches an LM Studio server that merely lists the same model.
- **Model-Aware Routing**: Automatically identifies the requested model from the request body and routes it only to backends that list that model. Among those, selection is load-aware: first to backends where the model is already loaded and ready (Ollama `/api/ps`, LM Studio loaded instances; vLLM-style backends that don't report loaded state count as ready when they list the model), then to cold backends with nothing loaded, and only as a last resort to a backend that would have to evict a different loaded model — so a request for `qwen3.8-27b` goes to the server that has it in GPU memory instead of making a busy server with another model load it. Name matching is deterministic and bounded — exact, `:tag`/case-insensitive, or publisher-/quant-suffixed variants of the same id (`qwen3.8-27b` reaches `unsloth/qwen3.8-27b@q8_0`) — but never arbitrary substrings (so it will not be routed to an unrelated `...-abliterated` build).
- **Smart Model Matching**: Robust matching that handles common variations like `:latest` tags and case-insensitivity. For example, a request for `llama3` will correctly match `llama3:latest` on the backend.
- **Parallel Processing**: Unlike basic proxies, `ollamaMQ` runs requests on every backend at once (up to each backend's slot count), significantly increasing throughput for multiple users.
- **Backend Health Checks**: Automatically monitors backend status every 10 seconds. All backends are probed **concurrently**, each request bounded by its own 5-second timeout, so one unresponsive backend can't hold up everyone else's status. Probes cover both API type (Ollama vs OpenAI) and the list of currently available models (via `/api/tags` and `/v1/models`). Endpoints a backend rejects are remembered and skipped until re-checked, so incompatible backends don't spam warnings. Offline instances are temporarily skipped and marked in the dashboard.
- **Fail-Fast for Unsatisfiable Requests**: If no online backend can ever serve a queued request (wrong API family or model absent everywhere), it is answered with `503` after `stuck_timeout` seconds instead of hanging forever. Requests merely waiting for a busy or loading backend are unaffected.
- **Per-Model Concurrency Limits**: Each model entry's `max_concurrent_requests` caps how many in-flight requests one backend may serve for that model, bounded globally by `settings.max_concurrent_per_backend` (default 1 — the historical one-request-per-backend behavior; raise it to let a single backend handle several requests at once). The limit is keyed on the model's base name, so it applies no matter how the client spells it — `qwen3.8-27b`, `qwen/qwen3.8-27b` and `unsloth/qwen3.8-27b@q8_0` all draw on the same budget as the configured entry.
- **Per-User Queuing**: Each user (identified by a client API key, else by the `X-User-ID` header, else by address as `ip-<address>`) has their own FIFO queue, capped at 100 waiting requests — beyond that the proxy answers `429` rather than growing the queue.
- **Bounded Memory**: A queued request holds its whole body until a backend takes it, so bodies are capped two ways: `settings.max_body_bytes` per request (default 64 MiB, `413` beyond it) and `settings.max_queued_bytes` across all queues at once (default 512 MiB, `503` beyond it). A request is always admitted when the queues are empty, so an oversized body can never lock the proxy out of making progress. `/api/blobs/{digest}` keeps a separate 1 GiB allowance for model-layer uploads.
- **Fair-Share Scheduling**: Prevents any single user from monopolizing all available backends. A user's turn is decided by their **recent** load: every dispatched request adds to a score that halves every 5 minutes, so a long-idle user isn't punished for past traffic and a newcomer can't monopolize the proxy while "catching up". Requests are charged when dispatched regardless of outcome, so a client whose requests always fail or disconnect can't keep jumping the queue.
- **Cache-Aware Hold-Back**: On a backend that reports KV-cache telemetry (llama.cpp/halogen with `--metrics`), a conversation whose last turn mostly *missed* that cache — and whose estimated next-turn prefill does not fit the free KV — lines up behind the conversations served most recently instead of forcing a context switch and a full re-prefill every round. A session that just finished a turn also keeps queue priority for its follow-up for ~5 s (adaptive: 1.5x the session's own worst recent pause, 2–30 s), so an agent runs its whole cycle on its backend undisturbed; at most 64 reserved wins in a row, and any held request yields at most 16 times, so neither mechanism can starve anyone. Plain Ollama (no telemetry) keeps the plain fair order.
- **VIP & Boost Modes**: Absolute priority (VIP) or increased frequency (Boost) for specific users.

## Sticky Routing (Session Affinity)

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

## Session Labels & Priority

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

## Cache-Aware Hold-Back

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

## Context Compaction

A coding agent that runs out of context does not start a new conversation: it asks the model to
summarize the one it has and continues from the summary. That summarization call is an ordinary
chat completion on this proxy, and on its own it reads as nonsense — pi hands it a fresh session
id and no `prompt_cache_key`, so a conversation that has been running for an hour arrives as a
brand-new session with an enormous prompt (one captured summary read 71 355 tokens and wrote
1 120) and a cold cache on a warm backend. The proxy recognizes those requests and puts them
where they belong.

**Recognition.** Nothing on the routes this proxy serves marks the call, so the marker is the
client's own prompt — one of these whole sentences, looked for in the first 32 KiB of the body
(every known marker sits in the first message; pi's sits at byte 73 of a real capture):

| client | its summarization prompt begins |
| --- | --- |
| pi, pi-web, OpenClaw | `You are a context summarization assistant. Your task is to read a conversation` |
| opencode | `You are a helpful AI assistant tasked with summarizing conversations.` |
| Claude Code, Cline, Roo Code | `Your task is to create a detailed summary of the conversation so far` |

Whole sentences, never keywords — the commit-message tool prompt in ordinary traffic says
*“Summarize the nature of the changes”*, and a mislabelled row is worse than a missing one. And
the sentence has to **open** a message (`"content":"You are a context…`), not merely appear in
one: a request that quotes a summarization prompt is ordinary traffic, and it cannot be mistaken
for the real thing, because text inside a message arrives JSON-escaped (`\"content\":\"You are…`)
and never reads as that boundary. That rule was earned the hard way — an operator debugging the
proxy pasted the pi marker into a `curl` example, and every later turn of that session carried it
in a tool call and filed itself as a compaction. The scan runs on the raw body in memory, so a
client whose body text is redacted is still recognized and nothing new is stored either way; a
client that knows what it is doing can skip the guess with **`X-Compaction: 1`**. Not recognized:
**OpenAI Codex CLI**, which appends its compaction prompt *after* the history — past any bounded
window. Its summaries arrive as ordinary requests.

**Attachment.** The summary belongs to the conversation it summarizes, and it has to be attached
before dispatch because the answer also decides where the request is queued. Candidates are
finished requests from the same authenticated client, the same key and the same model that were
active in the last 120 seconds (the socket is never part of it — one pi thread arrived over
`:48290` and its summary over `:50302`). One candidate wins; several need a clear winner: the
newest no more than 30 s old **and** the runner-up at least 3× older. Anything else stays an
explicitly **unlinked** row, because two live threads cannot be told apart by their clocks — and
not by their size either: that 285 KB summary compressed a 4.3 MB context, because clients
truncate tool results when they serialize a thread. Every attachment names how it was chosen
(`client header`, `recency`, `recency, clear among 3`), so an inferred link never reads as a
stated one. A client can settle it exactly with **`X-Compaction-Of: <session tag>`**; if that tag
matches nothing the guess is dropped rather than made anyway.

An attached summary takes the parent's backend affinity (its model is loaded there already), its
priority level — an inherited `top` also takes the one overbook slot that level buys — and its
conversation row and log entry.

**What it changes about the numbers.** A summary's cold prefill is not an eviction, so
compactions are left out of the cache-waste accounting; without that, every compaction charges
its backend tens of seconds of waste it never caused. The turn that resumes right after a
summary is stored as `post-compact`, so its cold prefill says why: the stage line reads
`cache 0% · cold after the summary` instead of looking like the backend dropped the context.
Per-client request and token totals are untouched — a compaction is real traffic and counts as
one request.

**Where it shows.** In Activity the summary is one striped graphite block on the parent's row
(no status hue or client hue was free, so it carries its meaning as texture, not as colour);
the conversation's label gains a `⤓`, and a summary nothing could be attached to keeps its own
row, labelled `compaction · unlinked`. The top bar counts `⤓ Compact N` — conversations that
compacted in the window, not summarization requests, since pi writes two per compaction (the
history, then the split turn's prefix). A Details line reads
`compaction 71k tok → 1.1k tok · recency`: what it read, what it left behind, how it was
attached. On the Logs page the request line carries `body=285694B · compaction`, the
conversation row a `⤓ N` badge, and the conversation viewer marks the turn itself — with its
text still redacted when the client asked for that.

In the request log a turn may carry `kind` (`compaction`, `post-compact`) and `link` (the
provenance string above); ordinary turns carry neither.
