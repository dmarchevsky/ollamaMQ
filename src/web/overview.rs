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
use crate::stats::{self, Agg, Finished, SPARK_BUCKETS, Summary, buckets, group_by, percentile};

/// Rows of the clients table and markers on the activity chart.
const MAX_CLIENTS: usize = 50;
const MAX_MARKERS: usize = 100;
const MAX_QUEUE: usize = 100;

#[derive(Deserialize)]
pub struct OverviewQuery {
    window: Option<String>,
}

#[derive(Serialize)]
pub struct OverviewView {
    window_secs: u64,
    now_ms: u64,
    status: StatusView,
    kpis: Kpis,
    timeline: Vec<stats::Sample>,
    markers: Vec<Marker>,
    backends: Vec<BackendStats>,
    clients: Vec<ClientStats>,
    queue: Vec<Waiting>,
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
}

#[derive(Serialize)]
struct LoadingView {
    backend: String,
    verb: String,
    model: String,
}

#[derive(Serialize)]
struct Kpis {
    #[serde(flatten)]
    totals: Summary,
    /// Requests that reached a backend, per minute.
    req_per_min: f64,
    error_pct: Option<f64>,
    /// Generated tokens per second over the window, all streams together.
    out_tps: Option<f64>,
    spark: Sparks,
}

#[derive(Serialize)]
struct Sparks {
    running: Vec<Option<f64>>,
    queued: Vec<Option<f64>>,
    req_per_min: Vec<Option<f64>>,
    out_tps: Vec<Option<f64>>,
    ttft: Vec<Option<f64>>,
    gen_tps: Vec<Option<f64>>,
}

#[derive(Serialize)]
struct Marker {
    at_ms: u64,
    outcome: &'static str,
    /// Backend HTTP status, for errors the backend answered with a 5xx.
    status: Option<u16>,
    client: String,
    model: String,
    backend: Option<String>,
}

#[derive(Serialize)]
struct BackendStats {
    idx: usize,
    label: String,
    online: bool,
    api_type: &'static str,
    running: usize,
    max: u32,
    free: usize,
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
}

fn window_secs(w: Option<&str>) -> u64 {
    match w {
        Some("5m") => 300,
        Some("1h") => 3600,
        _ => 900,
    }
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    percentile(&mut v, 50.0)
}

pub async fn get_overview(
    State(state): State<Arc<AppState>>,
    Query(q): Query<OverviewQuery>,
) -> Json<OverviewView> {
    let window = window_secs(q.window.as_deref());
    let now = stats::now_ms();
    let from = now.saturating_sub(window * 1000);
    let bucket_secs = window as f64 / SPARK_BUCKETS as f64;
    let finished = state.stats.finished_since(from);
    let samples = state.stats.samples_since(from);
    let snap = snapshot::capture(&state, 0);
    let runs = state.runs.list();

    // Waiting requests, copied out under the queues lock and labelled
    // afterwards (that takes the backends lock).
    let waiting: Vec<(String, Option<String>, f64, usize)> = state
        .queues
        .lock_or_recover()
        .iter()
        .flat_map(|(client, q)| {
            q.iter().enumerate().map(move |(i, t)| {
                (client.clone(), t.requested_model.clone(), t.queued_at.elapsed().as_secs_f64(), i + 1)
            })
        })
        .collect();
    let policy = state.model_policy();
    let mut queue: Vec<Waiting> = {
        let backends = state.backends.lock_or_recover();
        waiting
            .into_iter()
            .map(|(client, model, waited_secs, pos)| Waiting {
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
                online: b.is_online,
                api_type: b.api_type.display(),
                running: b.active_requests,
                max: state.backend_capacity(b).0,
                free: state.free_slots(b, loading_at(idx).is_some()),
                loading: loading_at(idx),
                stats: by_backend.get(&label).cloned().unwrap_or_default().summary(),
                spark,
                label,
            }
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

    // --- totals and sparklines
    let served: Vec<&Finished> = finished.iter().filter(|f| f.backend.is_some()).collect();
    let mut all = Agg::default();
    finished.iter().for_each(|f| all.add(f));
    let totals = all.summary();
    let sampled_secs = samples.len() as f64 * stats::SAMPLE_EVERY.as_secs_f64();
    let out_tps = (sampled_secs > 0.0)
        .then(|| samples.iter().map(|s| s.out_tokens).sum::<u64>() as f64 / sampled_secs);
    let by_sample = |f: fn(&stats::Sample) -> f64, combine: fn(&[f64]) -> f64| {
        buckets(from, now, samples.iter().map(|s| (s.at_ms, f(s))))
            .into_iter()
            .map(|v| (!v.is_empty()).then(|| combine(&v)))
            .collect::<Vec<_>>()
    };
    let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let max = |v: &[f64]| v.iter().copied().fold(0.0, f64::max);
    let spark = Sparks {
        running: by_sample(|s| s.running as f64, avg),
        queued: by_sample(|s| s.queued as f64, max),
        out_tps: by_sample(|s| s.out_tokens as f64, |v| v.iter().sum::<f64>() / (v.len() as f64 * stats::SAMPLE_EVERY.as_secs_f64())),
        req_per_min: buckets(from, now, served.iter().map(|f| (f.at_ms, ())))
            .into_iter()
            .map(|v| Some(v.len() as f64 * 60.0 / bucket_secs))
            .collect(),
        ttft: buckets(from, now, finished.iter().filter_map(|f| Some((f.at_ms, f.timing.ttft?))))
            .into_iter()
            .map(median)
            .collect(),
        gen_tps: buckets(from, now, finished.iter().filter_map(|f| Some((f.at_ms, f.gen_speed()?))))
            .into_iter()
            .map(median)
            .collect(),
    };
    let kpis = Kpis {
        req_per_min: served.len() as f64 * 60.0 / window as f64,
        error_pct: (totals.requests > 0)
            .then(|| totals.errors as f64 * 100.0 / totals.requests as f64),
        out_tps,
        spark,
        totals,
    };

    let mut markers: Vec<Marker> = finished
        .iter()
        .rev()
        .filter(|f| f.is_error())
        .take(MAX_MARKERS)
        .map(|f| Marker {
            at_ms: f.at_ms,
            outcome: f.outcome,
            status: f.timing.status.filter(|s| *s >= 500),
            client: f.client.clone(),
            model: f.model.clone(),
            backend: f.backend.clone(),
        })
        .collect();
    markers.reverse();

    let online: Vec<_> = backends.iter().filter(|b| b.online).collect();
    Json(OverviewView {
        window_secs: window,
        now_ms: now,
        status: StatusView {
            backends_total: backends.len(),
            backends_online: online.len(),
            capacity: online.iter().map(|b| b.max).sum(),
            running: snap.users.iter().map(|u| u.processing).sum(),
            free: online.iter().map(|b| b.free).sum(),
            queued: queued_total,
            oldest_wait_secs,
            loading: snap.backends.iter().enumerate().filter_map(|(i, _)| loading_at(i)).collect(),
        },
        kpis,
        timeline: samples,
        markers,
        backends,
        clients,
        queue,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_names() {
        assert_eq!(window_secs(Some("5m")), 300);
        assert_eq!(window_secs(Some("1h")), 3600);
        assert_eq!(window_secs(Some("15m")), 900);
        assert_eq!(window_secs(Some("bogus")), 900);
        assert_eq!(window_secs(None), 900);
    }
}
