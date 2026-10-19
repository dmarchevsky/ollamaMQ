# Cache-aware hold-back — replay results

`cache_holdback_sim.py` replays request traces against a model of the serial
dispatch loop (single backend, fair-share RR with the 5-min half-life, LRU KV
prefix residency) and compares:

- **A — before**: plain fair-share round-robin (behaviour on `master`).
- **B — after**: the shipped rule — a conversation yields while (a) its last
  prefill on this backend was measured below 50% hit, (b) that backend reported
  KV usage, (c) at least `CACHE_GRACE=2` other conversations were served more
  recently, and (d) it has yielded fewer than `CACHE_HOLD_PASSES=16` times.
  First/second-turn conversations are never held (a fresh miss proves nothing).

`simulate(..., dyn_gap)` models agent conversations whose next turn arrives a
fixed gap after their own response (the observed behaviour); otherwise arrivals
come from the log. Memory of the measured hit survives eviction exactly like
the real `conv_cache` (the proxy cannot see evictions).

## The screenshot incident (3 agent sessions, ~145k-token contexts, KV fits 2)

The Brainiac case: 3 conversations totalling 478k tokens — exactly the "Total
Context 478k" in the dashboard screenshot. Synthetic turns arrive 5 s after
each response, serialized on one backend.

| | re-prefills | avoidable prefill | wait mean |
|---|---|---|---|
| A before | 42 / 60 | 6.3M tok (3589 GPU s) | 187 s |
| B after  | 10 / 60 | 1.5M tok ( 849 GPU s) |  67 s |

**−76% avoidable prefill, −71% total prefill, mean queue wait 187 s → 67 s.**
The four-agent variant (working set ~25% over capacity, harsher thrash) still
gives −18% prefill / −20% waste.

## Real busy hour (`logs-remote/logs/`, 484 requests, one backend, ~100% busy)

Organic traffic replayed at its logged arrival times; prompt bytes → tokens at
3.6 B/token; KV capacity and prefill rate swept because the log records neither:

| KV size | prefill rate | prefill saved | avoidable waste saved | wait mean |
|---|---|---|---|---|
| 600 MB-equiv | 40k t/s | −7.0% | −10.8% (78 GPU s) | 869 → 862 s |
| 800 MB-equiv | 40k t/s | −8.1% | −12.9% (87 GPU s) | 856 → 847 s |
| 1200 MB-equiv | 40k t/s | −12.7% | −20.7% (131 GPU s) | 847 → 838 s |
| 800 MB-equiv | 20k t/s | −4.5% | −7.1% (96 GPU s) | 1128 → 1125 s |

Positive everywhere; the better the cache fit, the more the ordering matters.
The first version of this rule (time valve + KV-pressure threshold) measured
*worse* on this replay — the passes/grace formulation above is what the
numbers forced.

## How to reproduce

```
python3 cache_holdback_sim.py synthetic 1300 1750          # screenshot case (4 agents)
python3 cache_holdback_sim.py '<path>/ollamamq-requests.jsonl*' 800 40000
```

## Limits

- Token and byte models are estimates (3.6 B/token, SSE bytes ≈ 251 B/token
  out); absolute seconds are indicative, the A-vs-B *ratio on the same trace*
  is the real signal.
- The log contains no `timings` bodies and no `kv_cache_usage` series, so the
  residency model is LRU-in-token-bytes with a swept capacity, not the real
  halogen cache.
- Single-backend replay only; multi-backend fleets interact through the
  affinity map, which the hold-back leaves untouched.
