# Changelog

All notable changes to this fork of [ollamaMQ](https://github.com/Chleba/ollamaMQ)
since upstream `v0.5.0`.

## v0.9.2 — 2026-10-18

### Changed

- **Docker Hub is no longer published to.** GHCR (`ghcr.io/dmarchevsky/ollamamq`) is the only
  registry: the release workflow lost its Docker Hub login, so nothing uses `DOCKERHUB_USERNAME` /
  `DOCKERHUB_TOKEN` any more (delete them from the repo's Actions secrets). The Hub image stays as
  it is, frozen at `0.9.1` — an install that still pulls it has to switch to the GHCR name.
- `release-done` proves a release against GHCR now
  (`docker buildx imagetools inspect ghcr.io/dmarchevsky/ollamamq:{version}`) instead of the
  Docker Hub tag API.

## v0.9.1 — 2026-10-18

### Changed

- **Release images now live on GHCR: `ghcr.io/dmarchevsky/ollamamq`.** Same tags as before
  (`latest`, `<version>`, `<major>.<minor>`), published by the same tag-triggered workflow. GHCR
  needs no registry secrets (the workflow's own `GITHUB_TOKEN` logs in) and public packages there
  carry no anonymous pull limit — Docker Hub caps anonymous pulls at ~100 per 6 h per IP, which
  bites shared homelab and CI addresses. The shipped `docker-compose.yml` pulls from GHCR now.
- **Docker Hub keeps getting every tag** as `dmarchevsky/ollamamq`, so installs created before this
  release keep updating. New installs should use the GHCR name; the Hub mirror goes away once GHCR
  has shipped a release or two.
- Two `docker run` examples pointed at `chlebon/ollamamq`, an upstream namespace that never published
  this fork — they now name the GHCR image. The manual *Publishing to Docker Hub* section is gone:
  publishing happens by pushing a release tag, the workflow is the documentation.

## v0.9.0 — 2026-10-17

### Added

- **Client filter on the Overview.** A chip row under the top bar lists every client seen in the
  selected window; picking one scopes the counters, Capacity, the timeline and both request lists to
  that client alone (`GET /ui/api/overview?client=<id>`). Free slots, backend health and the slots
  themselves stay backend-wide — they belong to the backend, not to the client. The choice is
  remembered per browser.
- **One Activity panel with a per-conversation timeline.** The old Activity swimlanes and the
  Requests panel merged into a single panel. The graph draws one row per conversation, grouped under
  the backend it ran on: each request paints one block per stage — waiting, prefill, decode — placed
  where that stage began and as wide as it ran, so durations are readable as length and not only as
  numbers. Stage colours are fixed (they match the legend); client identity is the swatch by the name.
  Rows step down with the axis by session start, cap at 12 per backend with `+n older sessions
  hidden`, and a stage cut off by a window edge is clipped and marked `‹`.
- **Details view.** Under the graph, `List | Details` replaces the old grouping. Details threads each
  conversation: one line per stage event with its own timestamp and numbers (prompt size, prefill
  time, `cache 48%` for a warm prompt, tokens, seconds, tok/s), a `waiting` line with its own start
  for a queued request, an `idle` line where a conversation sat quiet for over 30s, and one line for
  a request that ended without serving (`client gone`, `blocked`). Clicking a timeline row opens
  Details at that conversation.
- **A readable Overview on a phone.** Request rows become three-line cards — client, `model → backend`,
  counts, timing — the panel header keeps its title and both switches on one line, and a
  client/session-keyed colour scheme keeps conversations distinguishable at 390px.
- **Version in Settings.** The Settings page prints the running ollamaMQ version, so a board and a
  bug report can be matched without shell access.

### Changed

- The top bar no longer shows **Drop** / **LogDrop**; dropped requests stay in the per-client table,
  the TUI and `/metrics`. The window selector's counters are Queue / Running / Done.
- `Active n | All n` carries the counts that used to be a sentence above the list, and the divider
  between running and finished rows is a hairline rather than a label.

### Fixed

- Stage timings on the graph were anchored from the wrong zero point: `ttft_secs`, `gen_secs` and
  `elapsed_secs` are measured from *dispatch*, while `waited_secs` happened before it, so a request
  that had queued was drawn in the wrong place. Stages now rebuild as
  `arrival = finish − waited − prefill − decode`.
## v0.8.1 — 2026-10-13

### Changed

- `docker-compose.yml` runs the published Docker Hub image (tracking `latest`) instead of
  building the checkout locally: `docker compose pull && docker compose up -d` upgrades to
  the newest release; pin a version tag to make upgrades deliberate again.

## v0.8.0 — 2026-10-13

### Changed

- **Overview redesign.** The KPI cards and Recent requests panel are gone. Capacity bars now
  size to each backend's slots with clearly visible free space; waiting rows share the bar's
  token scale so a queue that outgrows a backend is shown as overflow, not a hidden number.
  Activity became a per-backend swimlane of stacked, per-client histograms; buckets are aligned
  to the wall clock and show the busiest instant in the bucket, so a bar never exceeds the slot
  count and closed buckets never re-height.
- The window selector moved to the top bar. There **Done** and **Drop** count the selected window
  (completed / dropped), mirroring the lifetime counters they replace; Queue and Running stay live.
- The Requests table shows prefill as `size · s.s` (one decimal, ≈ while estimated), a new Gen
  time column, first token, and prefix-cache use for finished requests.

### Added

- **Request grouping.** Requests carry a short session tag — the client's `X-Session-Id`, its
  `prompt_cache_key`, or a fingerprint of the conversation — and the Requests table groups by it:
  *Ungrouped* (newest first), *Threads* (a header per session with its turns under a dotted lane),
  or *Chain* (Ungrouped's order with a git-graph-style lane threaded through it).

## v0.7.0 — 2026-10-13

### Added

- **Sticky routing (session affinity)**: a conversation is remembered by the backend that
  served its previous turn and prefers to go back there, so a long multi-turn prompt is not
  re-prefilled on a backend that has no KV cache for it. `settings.sticky_routing` picks the
  key — an explicit `X-Session-Id` / `prompt_cache_key`, a fingerprint of the conversation's first turn
  (default), or the client id; `sticky_ttl`, `sticky_lag` and `sticky_max_entries` bound the
  memory. It is a preference, not a pin: a busy, offline or no-longer-serving backend loses
  the conversation and the mapping follows the move. New counter
  `ollamamq_affinity_decisions_total{result}`; the payoff shows up as lower
  `ollamamq_prompt_eval_tokens_total` per client.

## v0.6.0 — 2026-10-12

First tagged release of the fork. Everything below is on top of upstream `v0.5.0`.

### Breaking (config layout)

- The web UI now edits `appconf.yaml` itself — the `settings.json` overlay and
  `admin.json` are gone. A leftover `settings.json` / `admin.json` next to the
  config is merged into `appconf.yaml` on first start and renamed to `.bak`.
  A UI save rewrites `appconf.yaml`, so file comments do not survive a save;
  the previous version is kept as `appconf.yaml.bak` for "Undo last save".
- The UI-changed password is stored as an argon2id hash at
  `settings.web_ui.password_hash` in `appconf.yaml` (wins over
  `OLLAMA_MQ_UI_PASSWORD` / `settings.web_ui.password`; delete it to recover).
- Client and model-policy stores are now hand-editable YAML next to
  `appconf.yaml`: `clients.yaml`, `models.yaml` (old `clients.json` /
  `models.json` convert automatically on first start and are kept as `.bak`).
  Hand-written files may omit ids/timestamps; keys may be bare strings and
  hide rules plain patterns.
- `settings.settings_file` / `settings.admin_file` no longer exist (ignored if
  present); defaults for `clients_file` / `models_file` are the `.yaml` names.
- Store files resolve from the folder holding `appconf.yaml`, so the whole
  configuration lives in one folder (`config/` in the default compose file);
  request logs stay in the working directory. The old layout in the working
  directory keeps being used and is reported at startup.

### Added

- Web UI at `/ui`: sign-in with session cookies (basic auth still works for
  scripts), overview dashboard with live capacity, activity and per-backend /
  per-client speed, waiting requests on the capacity board, clients and API-key
  management, model filtering page, backends page, settings editor, password
  change, favicon; mobile layout without horizontal scrolling.
- Clients and API keys: per-client keys (`omq-…` or any string), queue identity
  by key, VIP/boost persistence, disable/rename/move-key semantics.
- Model filtering: hide/allow glob rules per backend (`models.yaml`), hidden
  models answered `404`, merged model listings respect the rules.
- Prometheus `/metrics`: per-client and per-model requests, tokens, bytes,
  latency; free slots, queue by model, per-key tokens; time to first token and
  generation speed; `settings.metrics_token`.
- Model aliases (`settings.model_aliases`) and built-in Olla-style URL prefixes.
- Requests in progress with live token counts (`/api/ps`-style "Running now").
- Backend capacity auto-detected from reported slots (`detect_backend_slots`).
- Backend `name` and `disabled` fields; per-backend UI labels.
- Config-folder docker-compose becomes the default deployment: `config/` and
  `logs/` folders; the service runs without the TUI (`docker logs`).

### Fixed

- Metrics: per-request series start at zero (first values no longer lost);
  llama.cpp stream token counts from final-chunk timings.

### Upstream changes merged into this fork

- `kind: systemone` backend pinning and related Laya-style support (present in
  upstream `v0.5.0` history at the merge base).
