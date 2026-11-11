# Prometheus metrics

[← Back to README](../README.md)

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
| `ollamamq_audit_scanned_total` / `ollamamq_audit_dropped_total` | counter | turns the security audit judged; turns its queue had no room for (the audit fell behind rather than hold anything) |
| `ollamamq_audit_flags_total` | counter | kind (`injection` `exfil` `recon` `privesc` `harm`) — suspicious-activity flags raised; mark-only, nothing was ever blocked |
| `ollamamq_audit_backend_down_seconds_total` | counter | seconds the audit backend was not answering and the audit ran scanner-only |
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

**Cache waste.** `ollamamq_cache_waste_seconds_total` (counter; client, model, backend) is the prefill time lost re-evaluating a conversation's context after it was evicted from a backend's KV cache. The waste share is `rate(ollamamq_cache_waste_seconds_total[5m]) / rate(ollamamq_prompt_seconds_total[5m])`; only backends that report per-request prefill telemetry contribute.

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
