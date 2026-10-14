//! `GET /ui/api/overview?window=5m|15m|1h`: the Overview page's numbers,
//! computed from the rolling history in [`crate::stats`] plus the live state.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};

use crate::dispatcher::{AppState, model_label_in, model_routable};
use crate::lock::LockExt;
use crate::metrics::backend_label;
use crate::snapshot;
use crate::stats::{self, Agg, SPARK_BUCKETS, Summary, buckets, group_by};

/// Rows of the clients table and markers on the activity chart.
const MAX_CLIENTS: usize = 50;
const MAX_MARKERS: usize = 100;
const MAX_QUEUE: usize = 100;
/// Rows of the Requests table's finished list.
const MAX_RECENT: usize = 100;
/// Fixed lane bucket widths, chosen so the bar count stays ~30–60 and edges
/// land on round multiples: 5 min→10 s, 15 min→30 s, 1 h→60 s.
fn lane_bucket_secs(window_secs: u64) -> u64 {
    match window_secs {
        300 => 10,
        3600 => 60,
        _ => 30,
    }
}

#[derive(Deserialize)]
pub struct OverviewQuery {
    window: Option<String>,
}

#[derive(Serialize)]
pub struct OverviewView {
    window_secs: u64,
    now_ms: u64,
    /// Aligned start / width of the activity lane scale (bucket edges are
    /// multiples of the width since the epoch, so bars do not slide). Lanes
    /// and markers are placed on this scale.
    lane_from_ms: u64,
    lane_span_ms: u64,
    status: StatusView,
    backends: Vec<BackendStats>,
    clients: Vec<ClientStats>,
    queue: Vec<Waiting>,
    /// One swimlane per backend with activity in the window.
    lanes: Vec<Lane>,
    /// Finished requests, newest first — the "All" mode of the Requests table.
    recent: Vec<Recent>,
}

#[derive(Serialize)]
struct StatusView {
    backends_total: usize,
    backends_online: usize,
    /// Slots of the online backends.
    capacity: u32,
    running: usize,
    free: usize,
    queued: usize,
    oldest_wait_secs: Option<f64>,
    /// Model loads/unloads in progress.
    loading: Vec<LoadingView>,
    /// Finished requests that count as proxy/backend errors.
    errors: u64,
    error_pct: Option<f64>,
    /// Requests that completed in the window (the top bar's windowed `Done`).
    done: u64,
    /// Requests dropped in the window — same outcomes as the lifetime `Drop`
    /// counter: timeout, blocked, client gone, backend error (not 429s).
    dropped: u64,
}

#[derive(Serialize)]
struct LoadingView {
    backend: String,
    verb: String,
    model: String,
}

#[derive(Serialize)]
struct BackendStats {
    idx: usize,
    label: String,
    name: Option<String>,
    online: bool,
    api_type: &'static str,
    running: usize,
    max: u32,
    free: usize,
    /// Percent of the KV cache free, when the server reports it — the free
    /// segments of the capacity row scale by it (uniform split otherwise).
    kv_free_pct: Option<u8>,
    loading: Option<LoadingView>,
    #[serde(flatten)]
    stats: Summary,
    /// Generated tokens per second of requests finished in each bucket.
    spark: Vec<Option<f64>>,
}

#[derive(Serialize)]
struct ClientStats {
    id: String,
    registered: bool,
    running: usize,
    queued: usize,
    #[serde(flatten)]
    stats: Summary,
    /// Share of the slot time used in the window (running requests included).
    share: f64,
}

#[derive(Serialize)]
struct Waiting {
    client: String,
    model: String,
    waited_secs: f64,
    /// 1-based position in the client's own queue.
    pos: usize,
    /// Backends that list the model and serve it under the model policy.
    backends: Vec<usize>,
    /// Prompt tokens estimated from the request body (bytes ÷ 4).
    est_prompt_tokens: u64,
    /// Conversation tag for the session views; `None` when there is none.
    session: Option<String>,
}

/// One backend's swimlane: occupancy per time bucket, plus what to flag.
#[derive(Serialize)]
struct Lane {
    idx: usize,
    label: String,
    online: bool,
    free: usize,
    max: u32,
    /// Requests served or attempted in the window.
    requests: u64,
    /// Highest number of requests running at the same instant in the window.
    peak_run: u32,
    /// Highest number waiting at the same instant (0 most of the time).
    peak_wait: u32,
    /// One bucket per [`lane_bucket_secs`] width, oldest first; each holds the
    /// client segments to stack bottom-up as they were at the bucket's busiest
    /// instant (empty bucket = `[]`, so the axis stays full width).
    buckets: Vec<Vec<LaneSeg>>,
    markers: Vec<Marker>,
}

/// One client's contribution to one bucket; `running` stacks below `waiting`.
#[derive(Serialize, Debug, PartialEq)]
struct LaneSeg {
    client: String,
    running: u32,
    waiting: u32,
}

#[derive(Clone, Serialize)]
struct Marker {
    at_ms: u64,
    outcome: &'static str,
    /// Backend HTTP status, for errors the backend answered with a 5xx.
    status: Option<u16>,
    client: String,
    model: String,
}

/// One finished request for the Requests table's "All" mode.
#[derive(Serialize)]
struct Recent {
    at_ms: u64,
    client: String,
    model: String,
    backend: Option<String>,
    outcome: &'static str,
    /// [`Finished::is_error`] — the badge call, decided once server-side.
    error: bool,
    status: Option<u16>,
    waited_secs: Option<f64>,
    prompt_tokens: u64,
    prefill_secs: Option<f64>,
    prefill_tokens: Option<u64>,
    /// Seconds spent generating (decode) — the Gen time column.
    gen_secs: Option<f64>,
    /// Conversation tag of the finished request; `None` when there was none.
    session: Option<String>,
    /// Share of the prompt served from the prefix cache, 0–100.
    cache_pct: Option<u8>,
    ttft_secs: Option<f64>,
    completion_tokens: u64,
    gen_tps: Option<f64>,
}

fn window_secs(w: Option<&str>) -> u64 {
    match w {
        Some("5m") => 300,
        Some("1h") => 3600,
        _ => 900,
    }
}

/// Percent of the prompt that did not need prefill (prefix-cache hit).
/// `None` when the backend reported no usage at all.
fn cache_pct(prompt_tokens: u64, prefill_tokens: Option<u64>) -> Option<u8> {
    let evaluated = prefill_tokens?;
    (prompt_tokens > 0)
        .then(|| (100.0 * (1.0 - evaluated.min(prompt_tokens) as f64 / prompt_tokens as f64)).round() as u8)
}

/// One client occupying one backend between two moments.
#[derive(Clone, Debug)]
struct Occ {
    client: String,
    from_ms: u64,
    to_ms: u64,
    waiting: bool,
}

/// How many requests were active at one instant, per bucket: the busiest
/// moment inside the bucket (peak concurrency always happens at an interval
/// start or at the bucket's own start), counted per client as running and
/// waiting. Bucket edges are multiples of `w_ms` since the epoch so they do
/// not slide between polls. A bucket never shows more running than the
/// backend ever held at one time — sequential requests do not stack.
fn stack_buckets(from_ms: u64, w_ms: u64, n: usize, occ: &[Occ]) -> (Vec<Vec<LaneSeg>>, u32, u32) {
    // Sorted endpoints: the number active at t is starts≤t minus ends≤t, so
    // the candidate scan costs a binary search per start instead of a rescan
    // of every interval (a busy hour here holds tens of thousands).
    let mut starts: Vec<u64> = occ.iter().map(|o| o.from_ms).collect();
    let mut ends: Vec<u64> = occ.iter().map(|o| o.to_ms).collect();
    starts.sort_unstable();
    ends.sort_unstable();
    let active = |t: u64| starts.partition_point(|f| *f <= t) - ends.partition_point(|e| *e <= t);
    let mut peak_run = 0;
    let mut peak_wait = 0;
    let buckets = (0..n)
        .map(|i| {
            let t0 = from_ms + i as u64 * w_ms;
            let t1 = t0 + w_ms;
            // Peak concurrency happens when an interval starts (or at t0).
            let mut best_t = t0;
            let mut best = active(t0);
            let cs = starts.partition_point(|f| *f < t0);
            let ce = starts.partition_point(|f| *f < t1);
            for &f in &starts[cs..ce] {
                let x = active(f);
                if x > best {
                    best = x;
                    best_t = f;
                }
            }
            let mut per: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
            for o in occ {
                if o.from_ms <= best_t && best_t < o.to_ms {
                    let e = per.entry(o.client.as_str()).or_default();
                    if o.waiting {
                        e.1 += 1
                    } else {
                        e.0 += 1
                    }
                }
            }
            let bucket: Vec<LaneSeg> = per
                .iter()
                .map(|(client, (running, waiting))| LaneSeg {
                    client: (*client).to_string(),
                    running: *running,
                    waiting: *waiting,
                })
                .collect();
            peak_run = peak_run.max(bucket.iter().map(|s| s.running).sum());
            peak_wait = peak_wait.max(bucket.iter().map(|s| s.waiting).sum());
            bucket
        })
        .collect();
    (buckets, peak_run, peak_wait)
}

/// Finish one lane: `None` when the backend did nothing in the window.
/// `from_ms` is the aligned start of the first bucket; buckets are `w_ms` wide.
fn build_lane(
    idx: usize,
    label: &str,
    online: bool,
    free: usize,
    max: u32,
    requests: u64,
    occ: &[Occ],
    mut markers: Vec<Marker>,
    from_ms: u64,
    w_ms: u64,
    n: usize,
) -> Option<Lane> {
    markers.sort_by_key(|m| m.at_ms);
    if occ.is_empty() && markers.is_empty() {
        return None;
    }
    let (buckets, peak_run, peak_wait) = stack_buckets(from_ms, w_ms, n, occ);
    Some(Lane {
        idx,
        label: label.to_string(),
        online,
        free,
        max,
        requests,
        peak_run,
        peak_wait,
        buckets,
        markers,
    })
}

pub async fn get_overview(
    State(state): State<Arc<AppState>>,
    Query(q): Query<OverviewQuery>,
) -> Json<OverviewView> {
    let window = window_secs(q.window.as_deref());
    let now = stats::now_ms();
    let from = now.saturating_sub(window * 1000);
    let bucket_secs = window as f64 / SPARK_BUCKETS as f64;
    // Lane buckets are wall-clock aligned: fixed width per window, edges at
    // multiples of it, the last bucket the one containing `now`.
    let w_ms = lane_bucket_secs(window) * 1000;
    let n = (window * 1000 / w_ms) as usize;
    let lane_from = now / w_ms * w_ms - (n as u64 - 1) * w_ms;
    let finished = state.stats.finished_since(from);
    let snap = snapshot::capture(&state, 0);
    let runs = state.runs.list();

    // Waiting requests, copied out under the queues lock and labelled
    // afterwards (that takes the backends lock).
    let waiting: Vec<(String, Option<String>, f64, usize, usize, Option<String>)> = state
        .queues
        .lock_or_recover()
        .iter()
        .flat_map(|(client, q)| {
            q.iter().enumerate().map(move |(i, t)| {
                (
                    client.clone(),
                    t.requested_model.clone(),
                    t.queued_at.elapsed().as_secs_f64(),
                    i + 1,
                    t.body.len(),
                    t.session_key.as_ref().map(|k| k.tag()),
                )
            })
        })
        .collect();
    let policy = state.model_policy();
    let mut queue: Vec<Waiting> = {
        let backends = state.backends.lock_or_recover();
        waiting
            .into_iter()
            .map(|(client, model, waited_secs, pos, body_len, session)| Waiting {
                backends: model
                    .as_deref()
                    .map(|m| {
                        backends
                            .iter()
                            .enumerate()
                            .filter(|(_, b)| model_routable(m, &b.available_models) && policy.served(m, &b.url))
                            .map(|(i, _)| i)
                            .collect()
                    })
                    .unwrap_or_default(),
                model: model_label_in(&backends, model.as_deref()),
                est_prompt_tokens: (body_len / 4) as u64,
                session,
                client,
                waited_secs,
                pos,
            })
            .collect()
    };
    queue.sort_by(|a, b| b.waited_secs.total_cmp(&a.waited_secs));
    let oldest_wait_secs = queue.first().map(|w| w.waited_secs);
    let queued_total = queue.len();
    queue.truncate(MAX_QUEUE);

    // --- backends
    let by_backend = group_by(&finished, |f| f.backend.clone());
    let loading_at = |idx: usize| {
        snap.active_ops.iter().find(|op| op.backend_idx == idx).map(|op| LoadingView {
            backend: backend_label(&snap.backends[idx].url),
            verb: op.verb.clone(),
            model: op.model.clone(),
        })
    };
    let backends: Vec<BackendStats> = snap
        .backends
        .iter()
        .enumerate()
        .map(|(idx, b)| {
            let label = backend_label(&b.url);
            let spark = buckets(
                from,
                now,
                finished
                    .iter()
                    .filter(|f| f.backend.as_deref() == Some(label.as_str()))
                    .map(|f| (f.at_ms, f.timing.completion_tokens)),
            )
            .into_iter()
            .map(|v| Some(v.iter().sum::<u64>() as f64 / bucket_secs))
            .collect();
            BackendStats {
                idx,
                name: b.name.clone(),
                online: b.is_online,
                api_type: b.api_type.display(),
                running: b.active_requests,
                max: state.backend_capacity(b).0,
                free: state.free_slots(b, loading_at(idx).is_some()),
                kv_free_pct: b.kv_free_pct,
                loading: loading_at(idx),
                stats: by_backend.get(&label).cloned().unwrap_or_default().summary(),
                spark,
                label,
            }
        })
        .collect();

    // --- activity swimlanes
    // One occurrence list per backend index.
    let mut occ: Vec<Vec<Occ>> = vec![Vec::new(); snap.backends.len()];
    let mut markers: Vec<Vec<Marker>> = vec![Vec::new(); snap.backends.len()];
    let mut requests: Vec<u64> = vec![0; snap.backends.len()];
    let idx_of_label: HashMap<&str, usize> = backends.iter().map(|b| (b.label.as_str(), b.idx)).collect();
    for f in &finished {
        let Some(idx) = f.backend.as_deref().and_then(|l| idx_of_label.get(l).copied()) else {
            continue;
        };
        requests[idx] += 1;
        if let Some(dur) = f.timing.duration {
            let run_end = f.at_ms;
            let run_start = run_end.saturating_sub((dur * 1000.0) as u64);
            let wait_ms = (f.queue_wait.unwrap_or(0.0) * 1000.0) as u64;
            if wait_ms > 0 {
                occ[idx].push(Occ {
                    client: f.client.clone(),
                    from_ms: run_start.saturating_sub(wait_ms),
                    to_ms: run_start,
                    waiting: true,
                });
            }
            occ[idx].push(Occ {
                client: f.client.clone(),
                from_ms: run_start,
                to_ms: run_end,
                waiting: false,
            });
        }
        if f.is_error() && markers[idx].len() < MAX_MARKERS {
            markers[idx].push(Marker {
                at_ms: f.at_ms,
                outcome: f.outcome,
                status: f.timing.status.filter(|s| *s >= 500),
                client: f.client.clone(),
                model: f.model.clone(),
            });
        }
    }
    for r in &runs {
        let Some(&idx) = idx_of_label.get(r.backend.as_str()) else {
            continue;
        };
        requests[idx] += 1;
        let start = now.saturating_sub((r.started.elapsed().as_secs_f64() * 1000.0) as u64);
        occ[idx].push(Occ {
            client: r.client.clone(),
            from_ms: start,
            to_ms: now,
            waiting: false,
        });
        let wait_ms = (r.waited_secs * 1000.0) as u64;
        if wait_ms > 0 {
            occ[idx].push(Occ {
                client: r.client.clone(),
                from_ms: start.saturating_sub(wait_ms),
                to_ms: start,
                waiting: true,
            });
        }
    }
    // Waiting now: shown on the one backend that could take it (requests any
    // backend could take stay in the Capacity panel only).
    for w in &queue {
        if w.backends.len() == 1 {
            let idx = w.backends[0];
            occ[idx].push(Occ {
                client: w.client.clone(),
                from_ms: now.saturating_sub((w.waited_secs * 1000.0) as u64),
                to_ms: now,
                waiting: true,
            });
        }
    }
    let lanes: Vec<Lane> = backends
        .iter()
        .filter_map(|b| {
            build_lane(
                b.idx,
                &b.label,
                b.online,
                b.free,
                b.max,
                requests[b.idx],
                &occ[b.idx],
                std::mem::take(&mut markers[b.idx]),
                lane_from,
                w_ms,
                n,
            )
        })
        .collect();

    // --- requests: recently finished, newest first
    let recent: Vec<Recent> = finished
        .iter()
        .rev()
        .take(MAX_RECENT)
        .map(|f| Recent {
            at_ms: f.at_ms,
            error: f.is_error(),
            status: f.timing.status,
            waited_secs: f.queue_wait,
            prompt_tokens: f.timing.prompt_tokens,
            prefill_secs: f.timing.prefill.map(|(s, _)| s),
            prefill_tokens: f.timing.prefill.map(|(_, t)| t),
            gen_secs: f.timing.decode.map(|(s, _)| s),
            session: f.session.clone(),
            cache_pct: cache_pct(f.timing.prompt_tokens, f.timing.prefill.map(|(_, t)| t)),
            ttft_secs: f.timing.ttft,
            completion_tokens: f.timing.completion_tokens,
            gen_tps: f.gen_speed(),
            client: f.client.clone(),
            model: f.model.clone(),
            backend: f.backend.clone(),
            outcome: f.outcome,
        })
        .collect();

    // --- clients: slot time of finished requests plus, for those still
    // running, the part of their run inside the window.
    let mut by_client: BTreeMap<String, Agg> = group_by(&finished, |f| Some(f.client.clone()));
    let mut running_secs: HashMap<String, f64> = HashMap::new();
    for r in &runs {
        let secs = r.started.elapsed().as_secs_f64().min(window as f64);
        *running_secs.entry(r.client.clone()).or_default() += secs;
    }
    for u in snap.users.iter().filter(|u| u.outstanding() > 0) {
        by_client.entry(u.id.clone()).or_default();
    }
    let mut clients: Vec<ClientStats> = by_client
        .into_iter()
        .map(|(id, agg)| {
            let mut stats = agg.summary();
            stats.busy_secs += running_secs.get(&id).copied().unwrap_or(0.0);
            let user = snap.users.iter().find(|u| u.id == id);
            ClientStats {
                registered: user.is_some_and(|u| u.registered),
                running: user.map_or(0, |u| u.processing),
                queued: user.map_or(0, |u| u.queued),
                share: 0.0,
                stats,
                id,
            }
        })
        .collect();
    let total_busy: f64 = clients.iter().map(|c| c.stats.busy_secs).sum();
    for c in &mut clients {
        c.share = if total_busy > 0.0 { c.stats.busy_secs / total_busy } else { 0.0 };
    }
    clients.sort_by(|a, b| {
        b.stats
            .busy_secs
            .total_cmp(&a.stats.busy_secs)
            .then((b.running + b.queued).cmp(&(a.running + a.queued)))
            .then(b.stats.requests.cmp(&a.stats.requests))
            .then(a.id.cmp(&b.id))
    });
    clients.truncate(MAX_CLIENTS);

    // --- totals
    let mut all = Agg::default();
    finished.iter().for_each(|f| all.add(f));
    let totals = all.summary();
    let done_window = finished.iter().filter(|f| f.outcome == "ok").count() as u64;
    // Mirrors the lifetime Drop counter (dropped_counts in the dispatcher):
    // dropped requests, not 429 refusals.
    let dropped_window = finished
        .iter()
        .filter(|f| matches!(f.outcome, "timeout" | "blocked" | "client_gone" | "backend_error"))
        .count() as u64;

    let online: Vec<_> = backends.iter().filter(|b| b.online).collect();
    Json(OverviewView {
        window_secs: window,
        now_ms: now,
        lane_from_ms: lane_from,
        lane_span_ms: n as u64 * w_ms,
        status: StatusView {
            backends_total: backends.len(),
            backends_online: online.len(),
            capacity: online.iter().map(|b| b.max).sum(),
            running: snap.users.iter().map(|u| u.processing).sum(),
            free: online.iter().map(|b| b.free).sum(),
            queued: queued_total,
            oldest_wait_secs,
            loading: snap.backends.iter().enumerate().filter_map(|(i, _)| loading_at(i)).collect(),
            errors: totals.errors,
            error_pct: (totals.requests > 0)
                .then(|| totals.errors as f64 * 100.0 / totals.requests as f64),
            done: done_window,
            dropped: dropped_window,
        },
        backends,
        clients,
        queue,
        lanes,
        recent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_names() {
        assert_eq!(window_secs(Some("5m")), 300);
        assert_eq!(window_secs(Some("15m")), 900);
        assert_eq!(window_secs(Some("1h")), 3600);
        assert_eq!(window_secs(Some("bogus")), 900);
        assert_eq!(window_secs(None), 900);
    }

    fn occ(client: &str, from_ms: u64, to_ms: u64, waiting: bool) -> Occ {
        Occ { client: client.into(), from_ms, to_ms, waiting }
    }

    #[test]
    fn lane_buckets_stack_running_below_waiting_per_client() {
        // 1 s buckets.
        let occ = vec![
            occ("a", 0, 32_000, false),      // running all window
            occ("b", 16_000, 32_000, false), // running second half
            occ("c", 0, 4_000, true),        // waiting first buckets only
        ];
        let (buckets, peak_run, peak_wait) = stack_buckets(0, 1_000, 32, &occ);
        assert_eq!(buckets.len(), 32);
        assert_eq!(
            buckets[0],
            vec![
                LaneSeg { client: "a".into(), running: 1, waiting: 0 },
                LaneSeg { client: "c".into(), running: 0, waiting: 1 },
            ]
        );
        assert_eq!(buckets[20], vec![
            LaneSeg { client: "a".into(), running: 1, waiting: 0 },
            LaneSeg { client: "b".into(), running: 1, waiting: 0 },
        ]);
        assert_eq!(peak_run, 2);
        assert_eq!(peak_wait, 1);
    }

    #[test]
    fn sequential_requests_do_not_stack_in_one_bucket() {
        // Five short requests back-to-back inside one 10 s bucket: they were
        // never running together, so the bar stays at 1.
        let occ = (0..5).map(|i| occ("a", 100 + i * 2_000, 1_100 + i * 2_000, false)).collect::<Vec<_>>();
        let (buckets, peak_run, _) = stack_buckets(0, 10_000, 3, &occ);
        assert_eq!(buckets[0], vec![LaneSeg { client: "a".into(), running: 1, waiting: 0 }]);
        assert_eq!(peak_run, 1);
    }

    #[test]
    fn waiting_stacks_above_running_at_the_busiest_instant() {
        let occ = vec![
            occ("a", 5_000, 6_000, false), // running in bucket 1
            occ("b", 5_500, 9_000, true),  // waiting longer, overlaps the run
        ];
        let (buckets, peak_run, peak_wait) = stack_buckets(0, 5_000, 2, &occ);
        assert_eq!(
            buckets[1],
            vec![
                LaneSeg { client: "a".into(), running: 1, waiting: 0 },
                LaneSeg { client: "b".into(), running: 0, waiting: 1 },
            ]
        );
        assert_eq!((peak_run, peak_wait), (1, 1));
    }

    #[test]
    fn lane_hidden_without_activity() {
        let no = build_lane(0, "b", true, 2, 4, 0, &[], Vec::new(), 0, 1000, 30);
        assert!(no.is_none());
        let with_marker = build_lane(
            0,
            "b",
            true,
            2,
            4,
            1,
            &[],
            vec![Marker {
                at_ms: 500,
                outcome: "timeout",
                status: None,
                client: "c".into(),
                model: "m".into(),
            }],
            0,
            1000,
            30,
        );
        assert!(with_marker.is_some_and(|l| l.peak_run == 0));
    }

    #[test]
    fn cache_pct_is_the_non_prefilled_share() {
        assert_eq!(cache_pct(100, Some(25)), Some(75));
        assert_eq!(cache_pct(100, Some(0)), Some(100));
        assert_eq!(cache_pct(100, Some(100)), Some(0));
        assert_eq!(cache_pct(100, Some(150)), Some(0)); // clamped, never negative
        assert_eq!(cache_pct(0, Some(0)), None);
        assert_eq!(cache_pct(100, None), None);
    }
}
