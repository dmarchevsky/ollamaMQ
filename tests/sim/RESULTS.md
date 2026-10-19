# Cache-aware hold-back — replay results

`cache_holdback_sim.py` replays request traces against a model of the serial
dispatch loop (single backend, fair-share RR with the 5-min half-life, LRU KV
prefix residency) and compares:

- **A — before**: plain fair-share round-robin (behaviour on `master`).
- **B — hold-back**: the cache rule — a conversation yields while (a) its last
  prefill on this backend was measured below 50% hit, (b) that backend reported
  KV usage, (c) at least `CACHE_GRACE=2` other conversations were served more
  recently, and (d) it has yielded fewer than `CACHE_HOLD_PASSES=16` times.
  First/second-turn conversations are never held (a fresh miss proves nothing).
- **C — shipped**: B plus the **slot reservation**: a session that finished a turn
  keeps queue priority for its own follow-up for an adaptive window — 1.5x the
  session's decaying worst turnaround, clamped 2–30 s (default 5 s unmeasured)
  — so an agent runs its whole cycle on its backend without another session
  interrupting. Turnarounds are measured from queue arrival minus previous
  completion, so queueing never inflates them; 64 reserved wins in a row force
  one ordinary fair round. B's hold additionally uses the **KV budget**: the
  hold triggers when the measured increment (evaluated + decoded last turn)
  does not fit the free KV, falling back to "cache not empty" only when the
  backend cannot size it. The window default is measured, not guessed: client
  turnaround in the request log is median 0.5 s, p99 4.1 s, max 4.3 s over 595
  continuations.

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
| B hold-back  | 10 / 60 | 1.5M tok ( 849 GPU s) |  67 s |

**−76% avoidable prefill, −71% total prefill, mean queue wait 187 s → 67 s.**
The four-agent variant (working set ~25% over capacity) still gives −18/−20%
for B; the reservation ties B there (with only 4 sessions, grace-2 already
hands the slot straight back to the session that just ran).

## Real busy hour (`logs-remote/logs/`, 484 requests, one backend, ~100% busy)

Organic traffic replayed at its logged arrival times; prompt bytes → tokens at
3.6 B/token; KV capacity and prefill rate swept because the log records neither.
**C (what ships) vs A:**

| KV size | prefill rate | prefill saved | avoidable waste saved | wait mean |
|---|---|---|---|---|
| 600 MB-equiv | 40k t/s | −43.7% | −67.6% (491 GPU s) | 869 → 683 s |
| 800 MB-equiv | 40k t/s | −50.4% | −80.3% (540 GPU s) | 856 → 674 s |
| 1200 MB-equiv | 40k t/s | −58.8% | −95.8% (607 GPU s) | 847 → 653 s |
| 800 MB-equiv | 20k t/s | −51.2% | −81.4% (1100 GPU s) | 1128 → 754 s |

The reservation is the dominant lever on organic traffic (B alone: −7 to −21%
waste): with sub-second client turnarounds, priority for the returning session
collapses the thrash to ~one miss per conversation (210 → 20 re-prefills at
800 MB). The first version of the hold-back (time valve + KV-pressure gate)
measured *worse* on this replay — the passes/grace formulation and the
reservation are what the numbers forced.

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
