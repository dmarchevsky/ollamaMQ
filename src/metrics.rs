//! Prometheus metrics at `GET /metrics` (text exposition format 0.0.4).
//!
//! Counters and histograms are recorded as requests flow through the proxy;
//! gauges (queues, backends, clients, model policy) are computed from live
//! state at scrape time. A small hand-rolled registry rather than a client
//! crate: the metric set is fixed, and owning the label handling lets us cap
//! cardinality — `client` and `model` values come from requests, so a client
//! inventing ids or model names must not grow the series set without bound.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use crate::dispatcher::AppState;
use crate::lock::LockExt;

/// Distinct values kept per capped label; further values report as `other`.
const MAX_LABEL_VALUES: usize = 200;

/// Tail of a response kept to find its token usage (it comes last).
pub const USAGE_TAIL_BYTES: usize = 16 * 1024;

pub struct CounterDef {
    pub name: &'static str,
    pub help: &'static str,
    pub labels: &'static [&'static str],
}

pub struct HistogramDef {
    pub name: &'static str,
    pub help: &'static str,
    pub labels: &'static [&'static str],
    pub buckets: &'static [f64],
}

pub const REQUESTS: CounterDef = CounterDef {
    name: "ollamamq_requests_total",
    help: "Requests by final outcome: ok, client_gone, backend_error, timeout, blocked, hidden_model, queue_full, overloaded.",
    labels: &["client", "model", "endpoint", "outcome"],
};
pub const BACKEND_RESPONSES: CounterDef = CounterDef {
    name: "ollamamq_backend_responses_total",
    help: "HTTP status codes returned by backends.",
    labels: &["client", "model", "backend", "code"],
};
pub const TOKENS: CounterDef = CounterDef {
    name: "ollamamq_tokens_total",
    help: "Tokens reported by backends in response usage (prompt / completion).",
    labels: &["client", "model", "backend", "kind"],
};
pub const REQUEST_BYTES: CounterDef = CounterDef {
    name: "ollamamq_request_bytes_total",
    help: "Request body bytes accepted into the queues.",
    labels: &["client", "model"],
};
pub const RESPONSE_BYTES: CounterDef = CounterDef {
    name: "ollamamq_response_bytes_total",
    help: "Response body bytes streamed back to clients.",
    labels: &["client", "model", "backend"],
};
pub const KEY_REQUESTS: CounterDef = CounterDef {
    name: "ollamamq_client_key_requests_total",
    help: "Requests authenticated by each client API key (id and label, never the key).",
    labels: &["client", "key_id", "key_label"],
};
pub const KEY_TOKENS: CounterDef = CounterDef {
    name: "ollamamq_client_key_tokens_total",
    help: "Tokens used through each client API key (key label, or id when unlabelled).",
    labels: &["client", "key", "kind"],
};
pub const CONTROL_OPS: CounterDef = CounterDef {
    name: "ollamamq_model_control_operations_total",
    help: "Model load/unload operations by result.",
    labels: &["backend", "model", "action", "result"],
};

pub const QUEUE_WAIT: HistogramDef = HistogramDef {
    name: "ollamamq_queue_wait_seconds",
    help: "Time requests waited in the queue before dispatch.",
    labels: &["client", "model"],
    buckets: &[
        0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
    ],
};
pub const TTFB: HistogramDef = HistogramDef {
    name: "ollamamq_time_to_first_byte_seconds",
    help: "Time from dispatch to the first response byte from the backend.",
    labels: &["model", "backend"],
    buckets: &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0],
};
pub const DURATION: HistogramDef = HistogramDef {
    name: "ollamamq_request_duration_seconds",
    help: "Time from dispatch to the end of the response.",
    labels: &["model", "backend"],
    buckets: &[
        0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    ],
};

const COUNTERS: &[&CounterDef] = &[
    &REQUESTS,
    &BACKEND_RESPONSES,
    &TOKENS,
    &REQUEST_BYTES,
    &RESPONSE_BYTES,
    &KEY_REQUESTS,
    &KEY_TOKENS,
    &CONTROL_OPS,
];
const HISTOGRAMS: &[&HistogramDef] = &[&QUEUE_WAIT, &TTFB, &DURATION];

#[derive(Default)]
struct HistValue {
    /// Per-bucket (non-cumulative) counts; one extra slot for +Inf.
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

#[derive(Default)]
struct Inner {
    counters: BTreeMap<&'static str, BTreeMap<Vec<String>, f64>>,
    histograms: BTreeMap<&'static str, BTreeMap<Vec<String>, HistValue>>,
    seen: BTreeMap<&'static str, HashSet<String>>,
    warned: HashSet<&'static str>,
}

impl Inner {
    /// Apply the cardinality cap to `client` and `model` label values.
    fn cap(&mut self, label: &'static str, value: &str) -> String {
        if !matches!(label, "client" | "model") {
            return value.to_string();
        }
        let seen = self.seen.entry(label).or_default();
        if seen.contains(value) {
            return value.to_string();
        }
        if seen.len() < MAX_LABEL_VALUES {
            seen.insert(value.to_string());
            return value.to_string();
        }
        if self.warned.insert(label) {
            tracing::warn!(
                "metrics: more than {} distinct '{}' values; further ones are reported as 'other'",
                MAX_LABEL_VALUES,
                label
            );
        }
        "other".to_string()
    }

    fn key(&mut self, labels: &'static [&'static str], values: &[&str]) -> Vec<String> {
        labels
            .iter()
            .zip(values)
            .map(|(l, v)| self.cap(l, v))
            .collect()
    }
}

/// The recorded (counter and histogram) metrics. A leaf lock: nothing else
/// is acquired while it is held, so it is safe to record under any state lock.
pub struct Metrics {
    inner: Mutex<Inner>,
    start_time: f64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            start_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        }
    }

    pub fn inc(&self, def: &'static CounterDef, values: &[&str], by: f64) {
        debug_assert_eq!(def.labels.len(), values.len(), "{}", def.name);
        let mut inner = self.inner.lock_or_recover();
        let key = inner.key(def.labels, values);
        *inner
            .counters
            .entry(def.name)
            .or_default()
            .entry(key)
            .or_insert(0.0) += by;
    }

    /// Create the series at zero without counting anything.
    ///
    /// `rate()`/`increase()` only see growth between scraped samples, so a
    /// series born already holding a value hides that first value. Series
    /// that a request will update when it *finishes* (tokens, duration, ok
    /// count) are touched when it is accepted or dispatched instead, so a
    /// long request's numbers are measured from a zero Prometheus has seen.
    pub fn touch(&self, def: &'static CounterDef, values: &[&str]) {
        self.inc(def, values, 0.0);
    }

    /// [`Self::touch`] for a histogram: buckets, sum and count at zero.
    pub fn touch_histogram(&self, def: &'static HistogramDef, values: &[&str]) {
        self.histogram(def, values, |_| {});
    }

    pub fn observe(&self, def: &'static HistogramDef, values: &[&str], v: f64) {
        self.histogram(def, values, |h| {
            let slot = def
                .buckets
                .iter()
                .position(|b| v <= *b)
                .unwrap_or(def.buckets.len());
            h.counts[slot] += 1;
            h.sum += v;
            h.count += 1;
        });
    }

    fn histogram(
        &self,
        def: &'static HistogramDef,
        values: &[&str],
        update: impl FnOnce(&mut HistValue),
    ) {
        debug_assert_eq!(def.labels.len(), values.len(), "{}", def.name);
        let mut inner = self.inner.lock_or_recover();
        let key = inner.key(def.labels, values);
        let h = inner
            .histograms
            .entry(def.name)
            .or_default()
            .entry(key)
            .or_default();
        if h.counts.is_empty() {
            h.counts = vec![0; def.buckets.len() + 1];
        }
        update(h);
    }

    fn render_recorded(&self, out: &mut String) {
        let inner = self.inner.lock_or_recover();
        for def in COUNTERS {
            header(out, def.name, def.help, "counter");
            if let Some(series) = inner.counters.get(def.name) {
                for (vals, v) in series {
                    sample(out, def.name, def.labels, vals, &[], *v);
                }
            }
        }
        for def in HISTOGRAMS {
            header(out, def.name, def.help, "histogram");
            let Some(series) = inner.histograms.get(def.name) else {
                continue;
            };
            let bucket = format!("{}_bucket", def.name);
            for (vals, h) in series {
                let mut cum = 0;
                for (i, b) in def.buckets.iter().enumerate() {
                    cum += h.counts[i];
                    sample(
                        out,
                        &bucket,
                        def.labels,
                        vals,
                        &[("le", fmt_f64(*b))],
                        cum as f64,
                    );
                }
                sample(
                    out,
                    &bucket,
                    def.labels,
                    vals,
                    &[("le", "+Inf".into())],
                    h.count as f64,
                );
                sample(
                    out,
                    &format!("{}_sum", def.name),
                    def.labels,
                    vals,
                    &[],
                    h.sum,
                );
                sample(
                    out,
                    &format!("{}_count", def.name),
                    def.labels,
                    vals,
                    &[],
                    h.count as f64,
                );
            }
        }
    }
}

// ---------------------------------------------------------------- exposition

fn header(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(
        out,
        "# HELP {name} {}",
        help.replace('\\', "\\\\").replace('\n', "\\n")
    );
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn fmt_f64(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn sample(
    out: &mut String,
    name: &str,
    labels: &[&str],
    values: &[String],
    extra: &[(&str, String)],
    v: f64,
) {
    out.push_str(name);
    let pairs: Vec<String> = labels
        .iter()
        .zip(values)
        .map(|(l, v)| format!("{l}=\"{}\"", escape(v)))
        .chain(extra.iter().map(|(l, v)| format!("{l}=\"{}\"", escape(v))))
        .collect();
    if !pairs.is_empty() {
        let _ = write!(out, "{{{}}}", pairs.join(","));
    }
    let _ = writeln!(out, " {}", fmt_f64(v));
}

/// One gauge family computed at scrape time.
fn gauge(out: &mut String, name: &str, help: &str, labels: &[&str], rows: Vec<(Vec<String>, f64)>) {
    header(out, name, help, "gauge");
    for (vals, v) in rows {
        sample(out, name, labels, &vals, &[], v);
    }
}

/// `host:port` of a backend URL, the `backend` label.
pub fn backend_label(url: &str) -> String {
    url.trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_string()
}

/// The whole `/metrics` body.
pub fn render(state: &Arc<AppState>) -> String {
    let mut out = String::with_capacity(16 * 1024);
    state.metrics.render_recorded(&mut out);

    let snap = crate::snapshot::capture(state, 0);
    let policy = state.model_policy();
    let clients = state.clients.lock_or_recover().list();
    let fair: Vec<(String, f64)> = {
        let now = std::time::Instant::now();
        state
            .fair_share
            .lock_or_recover()
            .iter()
            .map(|(u, s)| (u.clone(), s.value_at(now)))
            .collect()
    };
    let b = |v: bool| if v { "true" } else { "false" }.to_string();

    // Waiting requests by client and model. Copied out under the queues lock,
    // labelled afterwards (that takes the backends lock).
    let waiting: Vec<(String, Option<String>, f64)> = state
        .queues
        .lock_or_recover()
        .iter()
        .flat_map(|(client, q)| {
            q.iter().map(move |t| {
                (
                    client.clone(),
                    t.requested_model.clone(),
                    t.queued_at.elapsed().as_secs_f64(),
                )
            })
        })
        .collect();
    let mut queued: BTreeMap<Vec<String>, (f64, f64)> = BTreeMap::new();
    {
        let backends = state.backends.lock_or_recover();
        for (client, model, age) in waiting {
            let model = crate::dispatcher::model_label_in(&backends, model.as_deref());
            let e = queued.entry(vec![client, model]).or_insert((0.0, 0.0));
            e.0 += 1.0;
            e.1 = e.1.max(age);
        }
    }
    gauge(
        &mut out,
        "ollamamq_queue_depth",
        "Requests waiting in the queue, by client and model.",
        &["client", "model"],
        queued.iter().map(|(k, v)| (k.clone(), v.0)).collect(),
    );
    gauge(
        &mut out,
        "ollamamq_queue_oldest_seconds",
        "How long the oldest waiting request has been queued, by client and model.",
        &["client", "model"],
        queued.iter().map(|(k, v)| (k.clone(), v.1)).collect(),
    );
    gauge(
        &mut out,
        "ollamamq_requests_in_flight",
        "Requests being served for each client.",
        &["client"],
        snap.users
            .iter()
            .map(|u| (vec![u.id.clone()], u.processing as f64))
            .collect(),
    );
    // Runs in progress, grouped so each label set is one series.
    let mut active: BTreeMap<Vec<String>, (f64, f64, f64)> = BTreeMap::new();
    for r in state.runs.list() {
        let e = active
            .entry(vec![r.client, r.key, r.model, r.backend])
            .or_insert((0.0, 0.0, 0.0));
        e.0 += 1.0;
        e.1 = e.1.max(r.started.elapsed().as_secs_f64());
        e.2 += r.tokens as f64;
    }
    let labels = ["client", "key", "model", "backend"];
    gauge(
        &mut out,
        "ollamamq_active_requests",
        "Requests being served right now.",
        &labels,
        active.iter().map(|(k, v)| (k.clone(), v.0)).collect(),
    );
    gauge(
        &mut out,
        "ollamamq_active_oldest_seconds",
        "How long the oldest request in progress has been running.",
        &labels,
        active.iter().map(|(k, v)| (k.clone(), v.1)).collect(),
    );
    gauge(
        &mut out,
        "ollamamq_active_generated_tokens",
        "Tokens generated so far by requests in progress (estimated from the stream).",
        &labels,
        active.iter().map(|(k, v)| (k.clone(), v.2)).collect(),
    );
    gauge(
        &mut out,
        "ollamamq_queued_bytes",
        "Request body bytes waiting in all queues.",
        &[],
        vec![(
            vec![],
            state
                .queued_bytes
                .load(std::sync::atomic::Ordering::Relaxed) as f64,
        )],
    );

    let busy_with_control: HashSet<usize> = snap.active_ops.iter().map(|o| o.backend_idx).collect();
    let mut free = Vec::new();
    let mut up = Vec::new();
    let mut in_flight = Vec::new();
    let mut max_conc = Vec::new();
    let mut per_model = Vec::new();
    let mut model_counts = Vec::new();
    let mut loaded = Vec::new();
    for bk in &snap.backends {
        let label = backend_label(&bk.url);
        up.push((
            vec![label.clone(), bk.api_type.display().to_string()],
            if bk.is_online { 1.0 } else { 0.0 },
        ));
        in_flight.push((vec![label.clone()], bk.active_requests as f64));
        let (cap, detected) = state.backend_capacity(bk);
        max_conc.push((
            vec![
                label.clone(),
                if detected { "detected" } else { "config" }.to_string(),
            ],
            cap as f64,
        ));
        // What the scheduler could start now: none while offline or while a
        // model load/unload runs (it treats that backend as busy).
        let idx = snap
            .backends
            .iter()
            .position(|x| x.url == bk.url)
            .unwrap_or(usize::MAX);
        let slots = if !bk.is_online || busy_with_control.contains(&idx) {
            0
        } else {
            (cap as usize).saturating_sub(bk.active_requests)
        };
        free.push((vec![label.clone()], slots as f64));
        let mut models: Vec<_> = bk.active_by_model.iter().collect();
        models.sort();
        for (m, n) in models {
            per_model.push((vec![label.clone(), m.clone()], *n as f64));
        }
        let hidden = bk
            .available_models
            .iter()
            .filter(|m| !policy.served(m, &bk.url))
            .count();
        for (st, n) in [
            ("available", bk.available_models.len()),
            ("loaded", bk.loaded_models.len()),
            ("hidden", hidden),
        ] {
            model_counts.push((vec![label.clone(), st.to_string()], n as f64));
        }
        let mut res: Vec<_> = bk.loaded_models.iter().cloned().collect();
        res.sort();
        for m in res {
            loaded.push((vec![label.clone(), m], 1.0));
        }
    }
    gauge(
        &mut out,
        "ollamamq_backend_up",
        "1 when the backend answered its last health probe.",
        &["backend", "api_type"],
        up,
    );
    gauge(
        &mut out,
        "ollamamq_backend_requests_in_flight",
        "Requests a backend is serving now.",
        &["backend"],
        in_flight,
    );
    gauge(
        &mut out,
        "ollamamq_backend_free_slots",
        "Requests a backend could start now: capacity minus in-flight, 0 while offline or loading/unloading a model.",
        &["backend"],
        free,
    );
    let mut limits: Vec<(Vec<String>, f64)> = state
        .model_limits
        .lock_or_recover()
        .iter()
        .map(|(m, n)| (vec![m.clone()], *n as f64))
        .collect();
    limits.sort_by(|a, b| a.0.cmp(&b.0));
    gauge(
        &mut out,
        "ollamamq_model_max_concurrency",
        "Per-model in-flight cap per backend (models[].max_concurrent_requests).",
        &["model"],
        limits,
    );
    gauge(
        &mut out,
        "ollamamq_backend_max_concurrency",
        "In-flight request cap per backend; source = detected (slots the backend reports) or config (max_concurrent_per_backend).",
        &["backend", "source"],
        max_conc,
    );
    gauge(
        &mut out,
        "ollamamq_backend_model_requests_in_flight",
        "In-flight requests per backend and model.",
        &["backend", "model"],
        per_model,
    );
    gauge(
        &mut out,
        "ollamamq_backend_models",
        "Models per backend: available, loaded (resident), hidden (by the model policy).",
        &["backend", "state"],
        model_counts,
    );
    gauge(
        &mut out,
        "ollamamq_model_loaded",
        "1 for each model resident on a backend.",
        &["backend", "model"],
        loaded,
    );

    // client_info: registered clients (even idle) plus active unregistered ids.
    let registered: HashSet<&str> = clients.iter().map(|c| c.name.as_str()).collect();
    let mut info: Vec<(Vec<String>, f64)> = clients
        .iter()
        .map(|c| {
            (
                vec![
                    c.name.clone(),
                    b(true),
                    b(c.disabled),
                    b(snap.vip_user.as_deref() == Some(&c.name)),
                    b(snap.boost_user.as_deref() == Some(&c.name)),
                ],
                1.0,
            )
        })
        .collect();
    let mut unregistered_active = 0;
    for u in &snap.users {
        if registered.contains(u.id.as_str()) {
            continue;
        }
        unregistered_active += 1;
        info.push((
            vec![
                u.id.clone(),
                b(false),
                b(snap.blocked_users.contains(&u.id)),
                b(snap.vip_user.as_deref() == Some(&u.id)),
                b(snap.boost_user.as_deref() == Some(&u.id)),
            ],
            1.0,
        ));
    }
    gauge(
        &mut out,
        "ollamamq_client_info",
        "Always 1; labels describe each client (join on client).",
        &["client", "registered", "disabled", "vip", "boost"],
        info,
    );
    let mut fair = fair;
    fair.sort_by(|a, b| a.0.cmp(&b.0));
    gauge(
        &mut out,
        "ollamamq_client_fair_share_score",
        "Scheduler fair-share score (recent dispatched load, half-life 5 min).",
        &["client"],
        fair.into_iter().map(|(u, v)| (vec![u], v)).collect(),
    );
    let disabled = clients.iter().filter(|c| c.disabled).count();
    gauge(
        &mut out,
        "ollamamq_clients",
        "Clients by state.",
        &["state"],
        vec![
            (vec!["registered".into()], clients.len() as f64),
            (vec!["disabled".into()], disabled as f64),
            (
                vec!["unregistered_active".into()],
                unregistered_active as f64,
            ),
        ],
    );
    let blocked_ids = snap
        .blocked_users
        .difference(&snap.disabled_clients)
        .count();
    gauge(
        &mut out,
        "ollamamq_blocked",
        "Blocked IP addresses and ids.",
        &["kind"],
        vec![
            (vec!["ip".into()], snap.blocked_ips.len() as f64),
            (vec!["id".into()], blocked_ids as f64),
        ],
    );
    let count_rules = |a| policy.rules.iter().filter(|r| r.action == a).count() as f64;
    gauge(
        &mut out,
        "ollamamq_model_policy_rules",
        "Model filter rules by action.",
        &["action"],
        vec![
            (
                vec!["hide".into()],
                count_rules(crate::model_policy::Action::Hide),
            ),
            (
                vec!["allow".into()],
                count_rules(crate::model_policy::Action::Allow),
            ),
        ],
    );
    let allow = policy.mode == crate::model_policy::Mode::Allow;
    gauge(
        &mut out,
        "ollamamq_model_policy_mode",
        "1 for the active model policy mode.",
        &["mode"],
        vec![
            (vec!["deny".into()], if allow { 0.0 } else { 1.0 }),
            (vec!["allow".into()], if allow { 1.0 } else { 0.0 }),
        ],
    );

    header(
        &mut out,
        "ollamamq_request_log_dropped_total",
        "Request-log records dropped because the log writer fell behind.",
        "counter",
    );
    sample(
        &mut out,
        "ollamamq_request_log_dropped_total",
        &[],
        &[],
        &[],
        snap.reqlog_dropped as f64,
    );
    gauge(
        &mut out,
        "ollamamq_build_info",
        "Always 1; the version label is the running build.",
        &["version"],
        vec![(vec![env!("CARGO_PKG_VERSION").to_string()], 1.0)],
    );
    gauge(
        &mut out,
        "ollamamq_start_time_seconds",
        "Process start time, Unix seconds.",
        &[],
        vec![(vec![], state.metrics.start_time)],
    );
    out
}

// ------------------------------------------------------------------ tokens

/// Token usage at the end of a response body: `(prompt, completion)`.
///
/// Understands a single JSON body (OpenAI `usage`, Ollama
/// `prompt_eval_count`/`eval_count`), Ollama's NDJSON stream (the final line
/// carries the counts) and OpenAI-style SSE (`data:` lines; usage is in the
/// last chunk when the backend sends it). `tail` is the end of the body, so
/// its first line may be cut — only complete lines are parsed.
pub fn parse_usage(tail: &[u8]) -> Option<(u64, u64)> {
    let text = String::from_utf8_lossy(tail);
    let from = |v: &serde_json::Value| -> Option<(u64, u64)> {
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            let p = u.get("prompt_tokens").and_then(|x| x.as_u64());
            let c = u.get("completion_tokens").and_then(|x| x.as_u64());
            if p.is_some() || c.is_some() {
                return Some((p.unwrap_or(0), c.unwrap_or(0)));
            }
        }
        let p = v.get("prompt_eval_count").and_then(|x| x.as_u64());
        let c = v.get("eval_count").and_then(|x| x.as_u64());
        if p.is_some() || c.is_some() {
            return Some((p.unwrap_or(0), c.unwrap_or(0)));
        }
        // llama.cpp streams carry no `usage` (unless asked), but the last
        // chunk has `timings`: prompt = cached + newly evaluated tokens.
        let tm = v.get("timings").filter(|t| t.is_object())?;
        let field = |k: &str| tm.get(k).and_then(|x| x.as_u64());
        let (prompt_n, predicted_n) = (field("prompt_n"), field("predicted_n"));
        (prompt_n.is_some() || predicted_n.is_some()).then(|| {
            (
                prompt_n.unwrap_or(0) + field("cache_n").unwrap_or(0),
                predicted_n.unwrap_or(0),
            )
        })
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim())
        && let Some(u) = from(&v)
    {
        return Some(u);
    }
    // Last line first: usage arrives at the end of a stream.
    for line in text.lines().rev() {
        let line = line.trim();
        let json = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
        if json.is_empty() || json == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json)
            && let Some(u) = from(&v)
        {
            return Some(u);
        }
    }
    None
}

// ------------------------------------------------------------- live runs

/// One request being served right now.
#[derive(Clone, Debug)]
pub struct Run {
    pub client: String,
    /// API key label (or id); `none` for requests without a client key.
    pub key: String,
    pub model: String,
    pub backend: String,
    pub started: std::time::Instant,
    pub first_byte: Option<std::time::Instant>,
    /// Generated tokens so far, estimated from the stream (see
    /// [`estimate_stream_tokens`]).
    pub tokens: u64,
}

/// Requests in progress, for the live views (`/metrics` gauges, the web
/// UI's "Running now"). A leaf lock.
#[derive(Default)]
pub struct Runs {
    inner: Mutex<BTreeMap<u64, Run>>,
    seq: std::sync::atomic::AtomicU64,
}

impl Runs {
    /// Register a run; it is removed when the returned guard drops, so every
    /// exit path (finished, client gone, backend error, panic) clears it.
    pub fn start(&self, client: &str, key: &str, model: &str, backend: &str) -> RunGuard<'_> {
        let id = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.lock_or_recover().insert(
            id,
            Run {
                client: client.to_string(),
                key: key.to_string(),
                model: model.to_string(),
                backend: backend.to_string(),
                started: std::time::Instant::now(),
                first_byte: None,
                tokens: 0,
            },
        );
        RunGuard { runs: self, id }
    }

    /// Oldest first.
    pub fn list(&self) -> Vec<Run> {
        self.inner.lock_or_recover().values().cloned().collect()
    }
}

pub struct RunGuard<'a> {
    runs: &'a Runs,
    id: u64,
}

impl RunGuard<'_> {
    pub fn update(&self, f: impl FnOnce(&mut Run)) {
        if let Some(r) = self.runs.inner.lock_or_recover().get_mut(&self.id) {
            f(r);
        }
    }
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        self.runs.inner.lock_or_recover().remove(&self.id);
    }
}

/// Generated tokens in a streamed chunk, estimated as the number of
/// non-empty text fields it carries: OpenAI `content` / `reasoning_content`
/// deltas and `text` (completions), Ollama `content` / `response`. Streaming
/// servers send about one token per event, so this tracks a run while it is
/// going; the exact count from the final usage tops it up when it ends.
pub fn estimate_stream_tokens(chunk: &[u8]) -> u64 {
    const FIELDS: &[&[u8]] = &[b"content\":\"", b"\"response\":\"", b"\"text\":\""];
    let mut n = 0;
    for f in FIELDS {
        let mut from = 0;
        while let Some(pos) = chunk[from..].windows(f.len()).position(|w| w == *f) {
            let end = from + pos + f.len();
            // `"content":""` is an empty delta, not a token.
            if chunk.get(end) != Some(&b'"') {
                n += 1;
            }
            from = end;
        }
    }
    n
}

/// Keep the last [`USAGE_TAIL_BYTES`] of a streamed body. Trims in batches so
/// a stream of small chunks does not shift the buffer on every chunk.
pub fn push_tail(tail: &mut Vec<u8>, chunk: &[u8]) {
    tail.extend_from_slice(chunk);
    if tail.len() > 2 * USAGE_TAIL_BYTES {
        let cut = tail.len() - USAGE_TAIL_BYTES;
        tail.drain(..cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_histograms_render() {
        let m = Metrics::new();
        m.inc(&REQUESTS, &["alice", "llama3", "/api/chat", "ok"], 1.0);
        m.inc(&REQUESTS, &["alice", "llama3", "/api/chat", "ok"], 2.0);
        m.observe(&TTFB, &["llama3", "a:1"], 0.07);
        m.observe(&TTFB, &["llama3", "a:1"], 99.0);
        let mut out = String::new();
        m.render_recorded(&mut out);
        assert!(out.contains("# TYPE ollamamq_requests_total counter"));
        assert!(out.contains(
            "ollamamq_requests_total{client=\"alice\",model=\"llama3\",endpoint=\"/api/chat\",outcome=\"ok\"} 3"
        ));
        assert!(out.contains("ollamamq_time_to_first_byte_seconds_bucket{model=\"llama3\",backend=\"a:1\",le=\"0.05\"} 0"));
        assert!(out.contains("ollamamq_time_to_first_byte_seconds_bucket{model=\"llama3\",backend=\"a:1\",le=\"0.1\"} 1"));
        assert!(out.contains("ollamamq_time_to_first_byte_seconds_bucket{model=\"llama3\",backend=\"a:1\",le=\"+Inf\"} 2"));
        assert!(out.contains(
            "ollamamq_time_to_first_byte_seconds_sum{model=\"llama3\",backend=\"a:1\"} 99.07"
        ));
        assert!(out.contains(
            "ollamamq_time_to_first_byte_seconds_count{model=\"llama3\",backend=\"a:1\"} 2"
        ));
        // Families with no data still declare themselves.
        assert!(out.contains("# TYPE ollamamq_tokens_total counter"));
    }

    #[test]
    fn touch_creates_zero_series() {
        let m = Metrics::new();
        m.touch(&TOKENS, &["dm", "qwen", "b:1", "completion"]);
        m.touch_histogram(&DURATION, &["qwen", "b:1"]);
        let mut out = String::new();
        m.render_recorded(&mut out);
        assert!(out.contains(
            r#"ollamamq_tokens_total{client="dm",model="qwen",backend="b:1",kind="completion"} 0"#
        ));
        assert!(
            out.contains(
                r#"ollamamq_request_duration_seconds_count{model="qwen",backend="b:1"} 0"#
            )
        );
        assert!(out.contains(
            r#"ollamamq_request_duration_seconds_bucket{model="qwen",backend="b:1",le="+Inf"} 0"#
        ));
    }

    #[test]
    fn stream_token_estimate() {
        let sse = b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n";
        assert_eq!(
            estimate_stream_tokens(sse),
            2,
            "empty role delta does not count"
        );
        let ollama = b"{\"message\":{\"role\":\"assistant\",\"content\":\"He\"},\"done\":false}\n{\"response\":\"llo\",\"done\":false}\n";
        assert_eq!(estimate_stream_tokens(ollama), 2);
        assert_eq!(estimate_stream_tokens(b"{\"error\":\"x\"}"), 0);
    }

    #[test]
    fn runs_clear_on_drop_and_show_as_gauges() {
        let runs = Runs::default();
        {
            let g = runs.start("dm", "laptop", "qwen", "b:1");
            g.update(|r| r.tokens += 5);
            let l = runs.list();
            assert_eq!((l.len(), l[0].tokens), (1, 5));
        }
        assert!(runs.list().is_empty());
    }

    #[test]
    fn label_values_are_escaped() {
        let m = Metrics::new();
        m.inc(&REQUEST_BYTES, &["we\"ird\\id\n", "m"], 5.0);
        let mut out = String::new();
        m.render_recorded(&mut out);
        assert!(out.contains(r#"client="we\"ird\\id\n""#), "{out}");
    }

    #[test]
    fn client_and_model_labels_are_capped() {
        let m = Metrics::new();
        for i in 0..MAX_LABEL_VALUES + 5 {
            m.inc(&REQUEST_BYTES, &[&format!("c{i}"), "m"], 1.0);
        }
        let mut out = String::new();
        m.render_recorded(&mut out);
        assert!(out.contains("ollamamq_request_bytes_total{client=\"other\",model=\"m\"} 5"));
        // A value seen before the cap keeps its own series.
        m.inc(&REQUEST_BYTES, &["c0", "m"], 1.0);
        let mut out = String::new();
        m.render_recorded(&mut out);
        assert!(out.contains("ollamamq_request_bytes_total{client=\"c0\",model=\"m\"} 2"));
    }

    #[test]
    fn usage_from_each_response_shape() {
        // OpenAI / llama.cpp, non-streaming.
        let openai = br#"{"id":"x","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":34,"total_tokens":46}}"#;
        assert_eq!(parse_usage(openai), Some((12, 34)));
        // Ollama, non-streaming.
        let ollama = br#"{"model":"llama3","done":true,"prompt_eval_count":7,"eval_count":9}"#;
        assert_eq!(parse_usage(ollama), Some((7, 9)));
        // Ollama NDJSON stream; the tail starts mid-line.
        let ndjson = b"ent\":\"Hi\"},\"done\":false}\n{\"message\":{\"content\":\"!\"},\"done\":false}\n{\"done\":true,\"prompt_eval_count\":3,\"eval_count\":5}\n";
        assert_eq!(parse_usage(ndjson), Some((3, 5)));
        // OpenAI SSE with usage in the last chunk.
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":6}}\n\ndata: [DONE]\n\n";
        assert_eq!(parse_usage(sse), Some((4, 6)));
        // llama.cpp stream: no usage, but `timings` in the last chunk
        // (captured from a real llama.cpp b11347 server).
        let llama = b"data: {\"choices\":[{\"delta\":{\"content\":\"!\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"timings\":{\"cache_n\":13,\"prompt_n\":5,\"prompt_ms\":63.5,\"predicted_n\":4,\"predicted_ms\":36.9}}\n\ndata: [DONE]\n\n";
        assert_eq!(parse_usage(llama), Some((18, 4)));
        // SSE without usage: unknown, not zero.
        let bare = b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: [DONE]\n\n";
        assert_eq!(parse_usage(bare), None);
    }

    #[test]
    fn tail_keeps_the_end() {
        let mut t = Vec::new();
        for _ in 0..100 {
            push_tail(&mut t, &[b'x'; 1000]);
        }
        push_tail(&mut t, b"END");
        assert!(t.len() <= 2 * USAGE_TAIL_BYTES);
        assert!(t.len() >= USAGE_TAIL_BYTES);
        assert!(t.ends_with(b"END"));
    }
}
