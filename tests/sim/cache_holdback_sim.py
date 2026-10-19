#!/usr/bin/env python3
"""Replay a Brainiac request log through a model of the ollamaMQ scheduler and
measure KV-cache waste before (plain fair-share) vs after (cache-aware
hold-back, feat/cache-aware-dispatch).

Model: the log's backend is 100% busy, so the GPU is one serial server.
Conversations are client connections (ip:port); prompt bytes are the request
body size, growth between turns of a connection is what a cache hit prefills.
KV = capacity C bytes, LRU eviction. A request hits when its conversation's
context is resident, else it full-prefills and evicts LRU others.
Policy A = the shipped scheduler: fair-share RR, +1 charge per dispatch,
half-life 5 min. Policy B = A + cold_hold: while the backend reports < 25%
KV free, a conversation whose last prefill hit < 50% lines up behind the hot
ones, held at most 90 s (valve). Constants mirror src/dispatcher.rs.

Usage: python3 cache_holdback_sim.py <jsonl-glob> [--calibrate|--report C_KB]
"""
import json, glob, statistics, collections, sys

BPT       = 3.6      # bytes per token of request JSON (screenshot-calibrated:
                     # median 523KB body on this fleet ~ 145k tok)
PRE_BPS   = 1750 * BPT   # prefill bytes/s: UI showed 142k tok in 80.9 s
DEC_BPS   = 6500     # SSE stream bytes/s, fitted from the log itself: median
                     # OUT bytes/s of requests >3 s (251 B envelope per chunk)
HL        = 300.0    # fair-share half-life, s
HOT_PCT   = 50
PASSES    = 16       # a cold front task yields at most this many hot dispatches
GRACE     = 2        # last N services may still hold cache (LRU grace window)
RES       = 5.0      # slot reservation: measured client tool time (p100 4.3s)
MEM_TTL   = 300.0


def load(pattern):
    recs = []
    for f in sorted(glob.glob(pattern)):
        for line in open(f):
            try:
                r = json.loads(line)
            except Exception:
                continue
            if r['path'] == '/v1/chat/completions':
                recs.append(r)
    recs.sort(key=lambda r: r['ts'])
    pend = collections.defaultdict(list)
    pairs = []
    for r in recs:
        if r['dir'] == 'IN':
            pend[r['user']].append(r)
        else:
            q = pend.get(r['user'])
            if q:
                pairs.append((q.pop(0), r))
    conns = collections.defaultdict(list)
    real = []
    for i, o in pairs:
        conns[i['user']].append((i['ts'] / 1000.0, i['bytes'], o['bytes']))
        real.append((o['ts'] - i['ts']) / 1000.0)
    return conns, real


def simulate(conns, C, policy, pre_tps=1750.0, dyn_gap=None):
    PRE = pre_tps * BPT
    turns = []
    for u, v in conns.items():
        prev = 0
        for k, (t, pb, ob) in enumerate(v):
            turns.append([u, k, t, pb, ob, prev, 0])
            prev = pb
    turns.sort(key=lambda x: x[2])
    queues = {u: collections.deque() for u in conns}
    if dyn_gap:
        # agents pace themselves: turn k+1 arrives dyn_gap s after turn k's
        # response; only the first turn is scheduled up front
        plan = {}
        prevs = {}
        first = {}
        for tr in turns:
            plan.setdefault(tr[0], []).append(tr)
        for u in conns:
            prev_b = 0
            for k, tr in enumerate(plan[u]):
                tr[5] = prev_b
                prev_b = tr[3]
            first_t = plan[u][0][2]
            for tr in plan[u][1:]:
                tr[2] = None
            first[u] = plan[u][0]
            queues[u].append(plan[u][0])
        turns = sorted((first[u][2] for u in conns))
        ti = 0
        first_t_map = {u: first[u][2] for u in conns}

    resident, touch, memory = {}, {}, {}
    last_served = [None]
    served_order = []  # conversation recency, newest last
    charges = collections.defaultdict(list)
    free_at = 0.0
    ti = 0
    now = 0.0
    total_prefill_b = unavoidable_b = 0.0
    waits, durs = [], []
    misses = first_seen_n = 0
    first_seen = set()

    def score(u, now):
        ch = charges[u]
        while ch and now - ch[0] > 4 * HL:
            ch.pop(0)
        return sum(0.5 ** ((now - t) / HL) for t in ch)

    while ti < len(turns) or any(queues.values()):
        if not dyn_gap:
            while ti < len(turns) and turns[ti][2] <= now:
                queues[turns[ti][0]].append(turns[ti])
                ti += 1
        ready = [u for u, q in queues.items() if q and q[0][2] is not None and q[0][2] <= now]
        if not ready:
            if dyn_gap:
                nxt = [q[0][2] for q in queues.values() if q and q[0][2] is not None]
                if not nxt:
                    break
                now = max(now + 0.001, min(nxt))
                continue
            if ti >= len(turns):
                break
            now = max(now + 0.001, turns[ti][2])
            continue
        if free_at > now:
            now = free_at
            continue  # back to top: absorb new arrivals at the advanced time
        ready.sort(key=lambda u: (score(u, now), u))
        colds = set()
        if policy in ('B', 'C'):
            recency = [u for u in reversed(served_order)]
            def cold(u):
                t = queues[u][0]
                if t[6] >= PASSES:
                    return False
                if u in recency[:GRACE]:
                    return False  # recent service: cache may still hold it
                m = memory.get(u)
                # hold only a conversation that has completed >1 turn here and
                # whose last prefill here missed (a first-turn miss proves
                # nothing about cache contention)
                return (bool(m) and now - m[1] <= MEM_TTL and m[0] < HOT_PCT
                        and m[2] >= 2)
            hot = [u for u in ready if not cold(u)]
            colds = set(u for u in ready if cold(u))
            ready = hot + [u for u in ready if u in colds]
        if policy == 'C':
            # slot reservation: a session that finished within RES keeps the
            # head of the queue for its own follow-up
            res = [u for u in ready if u in touch and now - touch[u] <= RES]
            ready = res + [u for u in ready if u not in res]
        u = ready[0]
        if policy in ('B', 'C') and colds and u not in colds:
            for c in colds:
                if queues[c]:
                    queues[c][0][6] += 1
        _, k, t_arr, pb, ob, prev, _passes = queues[u].popleft()
        wait = now - t_arr
        hit = u in resident
        prefill_b = (pb - prev) if (hit and pb >= prev) else pb
        total_prefill_b += prefill_b
        if u not in first_seen:
            unavoidable_b += pb
            first_seen.add(u)
            first_seen_n += 1
        else:
            unavoidable_b += max(pb - prev, 0)
            if not hit:
                misses += 1
        charges[u].append(now)
        gpu = prefill_b / PRE + ob / DEC_BPS
        finish = now + gpu
        free_at = finish
        resident[u] = pb
        touch[u] = finish
        last_served[0] = u
        served_order.append(u)
        served_order[:] = [x for x in served_order[-8:]]
        prev_mem = memory.get(u)
        # turns carry across idle gaps (history, not freshness — mirrors
        # CACHE_HISTORY_MAX_AGE in the dispatcher)
        memory[u] = (round(100 * (1 - prefill_b / pb)) if pb else 100, finish,
                     (prev_mem[2] if prev_mem else 0) + 1)
        tot = sum(resident.values())
        while tot > C:
            victims = sorted((t, w) for w, t in touch.items() if w != u)
            if not victims:
                # one context alone exceeds the cache: nothing can stay
                resident.pop(u, None)
                touch.pop(u, None)
                tot = 0
                break
            _, v = victims[0]
            tot -= resident.pop(v)
            del touch[v]
            # NOTE: memory (measured hit %) survives eviction — the real
            # proxy cannot see evictions, it only remembers last service.
        now = finish
        if dyn_gap and k + 1 < len(plan[u]):
            nxt = plan[u][k + 1]
            nxt[2] = finish + dyn_gap
            queues[u].append(nxt)
        waits.append(wait)
        durs.append(wait + gpu)

    return dict(
        served=len(durs),
        prefill_tok=total_prefill_b / BPT,
        waste_tok=(total_prefill_b - unavoidable_b) / BPT,
        misses=misses,
        gpu_prefill_s=total_prefill_b / PRE,
        service_s=sum(durs),
        waste_gpu_s=(total_prefill_b - unavoidable_b) / PRE,
        mean_wait=sum(waits) / len(waits),
        p90_wait=sorted(waits)[int(0.9 * len(waits))],
        dur_med=statistics.median(durs),
        dur_p90=sorted(durs)[int(0.9 * len(durs))],
        dur_max=max(durs),
        first_n=first_seen_n,
    )


def synthetic(n=4, tok=145000, grow=700, turns=15, gap=0.5):
    """The screenshot incident, reconstructed: n agent sessions with ~145k
    token contexts, each continuing 5 s after its previous answer (decode
    ~350 tok), all serialized on one backend."""
    conns = {}
    for i in range(n):
        v = []
        t = i * 0.1
        p = tok
        for _ in range(turns):
            out_tok = 350
            dur = (0 if not v else grow / 1750) + out_tok / 37.0
            v.append((t, p * BPT, out_tok * 251.0))
            t += dur + gap
            p += grow
        conns['agent-%d' % i] = v
    return conns


def main():
    pat = sys.argv[1] if len(sys.argv) > 1 else 'logs-remote/logs/ollamamq-requests.jsonl*'
    if pat == 'synthetic':
        conns = synthetic()
        real = [1.0]
        dyn = 0.5
    else:
        conns, real = load(pat)
        dyn = None
    print('log: %d requests, %d conversations' % (len(real), len(conns)))
    print('real IN->OUT duration: median %.1fs p90 %.1fs max %.1fs' %
          (statistics.median(real), sorted(real)[int(.9*len(real))], max(real)))
    arr = [t for v in conns.values() for (t, _, _) in v]
    print('log span: %.0fs (backend was ~100%% busy -> real total service ~= span)'
          % (max(arr) - min(arr)))
    if len(sys.argv) > 2 and sys.argv[2] == '--fit':
        tgt = statistics.median(real), sorted(real)[int(.9*len(real))], max(real)
        print('target real dur med/p90/max: %.1f %.1f %.1f' % tgt)
        for pre in (2500, 5000, 10000, 20000, 40000):
            for C_kb in (400, 800, 1600, 3200):
                a = simulate(conns, C_kb * 1024, 'A', pre)
                err = abs(a['dur_med']-tgt[0])/tgt[0] + abs(a['dur_p90']-tgt[1])/tgt[1] + abs(a['dur_max']-tgt[2])/tgt[2]
                print('pre=%5d C=%5dKB: dur med %6.1f p90 %6.1f max %6.0f err %5.2f | waste %6.1fM tok misses %3d' %
                      (pre, C_kb, a['dur_med'], a['dur_p90'], a['dur_max'], err,
                       a['waste_tok']/1e6, a['misses']))
        return
    if len(sys.argv) > 2 and sys.argv[2] == '--calibrate':
        for C_kb in (300, 400, 500, 600, 800, 1000, 1200, 1500):
            a = simulate(conns, C_kb * 1024, 'A')
            print('C=%5dKB A: waste %7.1fM tok (%5.0f GPU s) misses %3d | total service %6.0fs | dur med %6.1f p90 %6.1f max %6.0f' %
                  (C_kb, a['waste_tok'] / 1e6, a['waste_gpu_s'], a['misses'],
                   a['service_s'], a['dur_med'], a['dur_p90'], a['dur_max']))
        return
    C_kb = int(sys.argv[2]) if len(sys.argv) > 2 else 600
    pre = float(sys.argv[3]) if len(sys.argv) > 3 else 1750.0
    C = C_kb * 1024
    a = simulate(conns, C, 'A', pre, dyn)
    b = simulate(conns, C, 'B', pre, dyn)
    c = simulate(conns, C, 'C', pre, dyn)
    for n, r in (('A before', a), ('B after ', b), ('C +reserve', c)):
        print('%s: prefill %7.1fM tok (%5.0f s GPU) waste %6.1fM tok (%4.0f s GPU) '
              're-prefills %3d | wait mean %5.1fs p90 %6.1fs | dur med %5.1f p90 %6.1f max %6.0f' %
              (n, r['prefill_tok'] / 1e6, r['gpu_prefill_s'], r['waste_tok'] / 1e6,
               r['waste_gpu_s'], r['misses'], r['mean_wait'], r['p90_wait'],
               r['dur_med'], r['dur_p90'], r['dur_max']))
    dp = 100 * (1 - c['prefill_tok'] / a['prefill_tok'])
    dw = 100 * (1 - c['waste_gpu_s'] / a['waste_gpu_s']) if a['waste_gpu_s'] else 0
    print('C vs A: prefill -%.1f%%, avoidable-waste -%.1f%% (%.0f GPU s saved), waits %.1fs -> %.1fs mean'
          % (dp, dw, a['waste_gpu_s'] - c['waste_gpu_s'], a['mean_wait'], c['mean_wait']))


if __name__ == '__main__':
    main()
