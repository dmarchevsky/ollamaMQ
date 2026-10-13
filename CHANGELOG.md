# Changelog

All notable changes to this fork of [ollamaMQ](https://github.com/Chleba/ollamaMQ)
since upstream `v0.5.0`.

## Unreleased

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
