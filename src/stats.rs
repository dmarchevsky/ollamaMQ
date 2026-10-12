//! Rolling history behind the web UI's Overview page.
//!
//! [`crate::metrics`] gives Prometheus cumulative counters and histograms;
//! the Overview needs "the last 15 minutes" without a Prometheus server. This
//! keeps the last hour in memory: one record per finished request and one
//! activity sample every [`SAMPLE_EVERY`]. Nothing here survives a restart.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;

use crate::lock::LockExt;

/// How far back the history goes (the longest Overview window).
pub const HISTORY: Duration = Duration::from_secs(3600);
/// Cap on finished-request records, whatever their age.
const MAX_FINISHED: usize = 20_000;
/// Interval between activity samples.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(5);
const MAX_SAMPLES: usize = (HISTORY.as_secs() / SAMPLE_EVERY.as_secs()) as usize;
/// Points in each sparkline.
pub const SPARK_BUCKETS: usize = 30;

/// What was measured while serving one request. Parts the backend or the
/// response shape did not allow measuring stay `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Timing {
    /// HTTP status from the backend.
    pub status: Option<u16>,
    /// Dispatch to the end of the response.
    pub duration: Option<f64>,
    /// Dispatch to the first generated token (streams), or the prompt time
    /// the backend reports (non-streamed responses).
    pub ttft: Option<f64>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Generation time and the tokens generated in it.
    pub decode: Option<(f64, u64)>,
    /// Prompt evaluation (prefill) time and the prompt tokens evaluated.
    pub prefill: Option<(f64, u64)>,
}

/// One request that left the proxy: served, failed, or refused.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub at_ms: u64,
    pub client: String,
    pub model: String,
    /// Metrics label of the backend; `None` when it was never dispatched.
    pub backend: Option<String>,
    /// The `outcome` label of `ollamamq_requests_total`.
    pub outcome: &'static str,
    /// Seconds it waited in the queue; `None` when refused before queuing.
    pub queue_wait: Option<f64>,
    pub timing: Timing,
}

impl Finished {
    /// Failures the operator should look at. Client disconnects and blocked
    /// clients are not errors of the proxy or its backends.
    pub fn is_error(&self) -> bool {
        matches!(
            self.outcome,
            "backend_error" | "timeout" | "queue_full" | "overloaded"
        ) || self.timing.status.is_some_and(|s| s >= 500)
    }

    /// Generated tokens per second. One token has no measurable rate.
    pub fn gen_speed(&self) -> Option<f64> {
        self.timing
            .decode
            .filter(|(secs, n)| *secs > 0.0 && *n >= 2)
            .map(|(secs, n)| n as f64 / secs)
    }
}

/// Proxy activity at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Sample {
    pub at_ms: u64,
    pub running: u32,
    pub queued: u32,
    /// Slots of the online backends.
    pub capacity: u32,
    /// Slots that could start a request now.
    pub free: u32,
    /// Tokens generated since the previous sample (running streams included).
    pub out_tokens: u64,
}

#[derive(Default)]
pub struct Stats {
    finished: Mutex<VecDeque<Finished>>,
    samples: Mutex<VecDeque<Sample>>,
    /// Generated tokens since start, counted as they stream.
    out_tokens: AtomicU64,
    /// `out_tokens` when the last sample was taken.
    sampled_tokens: AtomicU64,
}

impl Stats {
    pub fn record(&self, f: Finished) {
        let cutoff = f.at_ms.saturating_sub(HISTORY.as_millis() as u64);
        let mut q = self.finished.lock_or_recover();
        while q.front().is_some_and(|x| x.at_ms < cutoff) || q.len() >= MAX_FINISHED {
            q.pop_front();
        }
        q.push_back(f);
    }

    pub fn add_out_tokens(&self, n: u64) {
        self.out_tokens.fetch_add(n, Ordering::Relaxed);
    }

    /// Store an activity sample; its `out_tokens` is filled in here.
    pub fn sample(&self, mut s: Sample) {
        let total = self.out_tokens.load(Ordering::Relaxed);
        s.out_tokens = total.saturating_sub(self.sampled_tokens.swap(total, Ordering::Relaxed));
        let mut q = self.samples.lock_or_recover();
        if q.len() >= MAX_SAMPLES {
            q.pop_front();
        }
        q.push_back(s);
    }

    /// Requests finished at or after `from_ms`, oldest first.
    pub fn finished_since(&self, from_ms: u64) -> Vec<Finished> {
        let q = self.finished.lock_or_recover();
        let start = q.partition_point(|f| f.at_ms < from_ms);
        q.range(start..).cloned().collect()
    }

    /// Samples taken at or after `from_ms`, oldest first.
    pub fn samples_since(&self, from_ms: u64) -> Vec<Sample> {
        let q = self.samples.lock_or_recover();
        let start = q.partition_point(|s| s.at_ms < from_ms);
        q.range(start..).copied().collect()
    }
}

/// Take one activity sample every [`SAMPLE_EVERY`], for as long as the proxy runs.
pub async fn run_sampler(state: std::sync::Arc<crate::dispatcher::AppState>) {
    let mut tick = tokio::time::interval(SAMPLE_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        state.stats.sample(current(&state));
    }
}

/// Running and queued requests and slot counts right now. Each lock is
/// taken on its own.
fn current(state: &crate::dispatcher::AppState) -> Sample {
    let queued: usize = state.queues.lock_or_recover().values().map(VecDeque::len).sum();
    let running: usize = state.processing_counts.lock_or_recover().values().sum();
    let busy: std::collections::HashSet<usize> =
        state.control_ops.lock_or_recover().keys().copied().collect();
    let backends = state.backends.lock_or_recover();
    let mut sample = Sample {
        at_ms: now_ms(),
        running: running as u32,
        queued: queued as u32,
        ..Default::default()
    };
    for (idx, b) in backends.iter().enumerate().filter(|(_, b)| b.is_online) {
        sample.capacity += state.backend_capacity(b).0;
        sample.free += state.free_slots(b, busy.contains(&idx)) as u32;
    }
    sample
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Nearest-rank percentile (`p` in 0..=100); sorts `v`. `None` when empty.
pub fn percentile(v: &mut [f64], p: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
    Some(v[rank.min(v.len()) - 1])
}

/// Totals over a group of finished requests.
#[derive(Clone, Debug, Default)]
pub struct Agg {
    requests: u64,
    errors: u64,
    ttft: Vec<f64>,
    decode: Vec<f64>,
    wait: Vec<f64>,
    prompt_tokens: u64,
    completion_tokens: u64,
    prefill_secs: f64,
    prefill_tokens: u64,
    busy_secs: f64,
}

/// What the Overview shows for a group: a backend, a client, or everything.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Summary {
    pub requests: u64,
    pub errors: u64,
    pub ttft_p50: Option<f64>,
    pub ttft_p95: Option<f64>,
    /// Median generation speed of the requests (tokens/s per stream).
    pub gen_p50: Option<f64>,
    pub wait_p95: Option<f64>,
    /// Prompt tokens evaluated per second of prefill.
    pub prefill_tps: Option<f64>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Backend slot time the finished requests used.
    pub busy_secs: f64,
}

impl Agg {
    pub fn add(&mut self, f: &Finished) {
        let t = &f.timing;
        self.requests += 1;
        self.errors += f.is_error() as u64;
        self.ttft.extend(t.ttft);
        self.decode.extend(f.gen_speed());
        self.wait.extend(f.queue_wait);
        self.prompt_tokens += t.prompt_tokens;
        self.completion_tokens += t.completion_tokens;
        if let Some((secs, n)) = t.prefill.filter(|(s, n)| *s > 0.0 && *n > 0) {
            self.prefill_secs += secs;
            self.prefill_tokens += n;
        }
        self.busy_secs += t.duration.unwrap_or(0.0);
    }

    pub fn summary(mut self) -> Summary {
        Summary {
            requests: self.requests,
            errors: self.errors,
            ttft_p50: percentile(&mut self.ttft, 50.0),
            ttft_p95: percentile(&mut self.ttft, 95.0),
            gen_p50: percentile(&mut self.decode, 50.0),
            wait_p95: percentile(&mut self.wait, 95.0),
            prefill_tps: (self.prefill_secs > 0.0)
                .then(|| self.prefill_tokens as f64 / self.prefill_secs),
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            busy_secs: self.busy_secs,
        }
    }
}

/// Requests grouped by `key`; those it maps to `None` are left out.
pub fn group_by<K: Ord>(items: &[Finished], key: impl Fn(&Finished) -> Option<K>) -> BTreeMap<K, Agg> {
    let mut out: BTreeMap<K, Agg> = BTreeMap::new();
    for f in items {
        if let Some(k) = key(f) {
            out.entry(k).or_default().add(f);
        }
    }
    out
}

/// Split `[from_ms, to_ms)` into [`SPARK_BUCKETS`] equal spans and collect
/// each item's value into the span its time falls in.
pub fn buckets<T>(from_ms: u64, to_ms: u64, items: impl IntoIterator<Item = (u64, T)>) -> Vec<Vec<T>> {
    let mut out: Vec<Vec<T>> = (0..SPARK_BUCKETS).map(|_| Vec::new()).collect();
    let span = to_ms.saturating_sub(from_ms).max(1);
    for (at, v) in items {
        if at < from_ms || at >= to_ms {
            continue;
        }
        let i = ((at - from_ms) as u128 * SPARK_BUCKETS as u128 / span as u128) as usize;
        out[i.min(SPARK_BUCKETS - 1)].push(v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fin(at_ms: u64, client: &str, backend: &str, outcome: &'static str, timing: Timing) -> Finished {
        Finished {
            at_ms,
            client: client.into(),
            model: "llama3".into(),
            backend: Some(backend.into()),
            outcome,
            queue_wait: Some(1.0),
            timing,
        }
    }

    #[test]
    fn percentile_nearest_rank() {
        assert_eq!(percentile(&mut [], 50.0), None);
        assert_eq!(percentile(&mut [7.0], 95.0), Some(7.0));
        let mut v: Vec<f64> = (1..=20).rev().map(f64::from).collect();
        assert_eq!(percentile(&mut v, 50.0), Some(10.0));
        assert_eq!(percentile(&mut v, 95.0), Some(19.0));
        assert_eq!(percentile(&mut v, 100.0), Some(20.0));
    }

    #[test]
    fn history_is_pruned_by_age_and_window_reads_are_ordered() {
        let s = Stats::default();
        let hour = HISTORY.as_millis() as u64;
        s.record(fin(1_000, "a", "b:1", "ok", Timing::default()));
        s.record(fin(2_000, "a", "b:1", "ok", Timing::default()));
        // An hour after the first: it falls out, the second stays.
        s.record(fin(1_500 + hour, "a", "b:1", "ok", Timing::default()));
        let all = s.finished_since(0);
        assert_eq!(all.iter().map(|f| f.at_ms).collect::<Vec<_>>(), vec![2_000, 1_500 + hour]);
        assert_eq!(s.finished_since(2_001).len(), 1);
    }

    #[test]
    fn samples_carry_tokens_generated_since_the_previous_one() {
        let s = Stats::default();
        s.add_out_tokens(40);
        s.sample(Sample { at_ms: 5_000, ..Default::default() });
        s.add_out_tokens(2);
        s.sample(Sample { at_ms: 10_000, ..Default::default() });
        let got: Vec<u64> = s.samples_since(0).iter().map(|x| x.out_tokens).collect();
        assert_eq!(got, vec![40, 2]);
        assert_eq!(s.samples_since(6_000).len(), 1);
    }

    #[test]
    fn groups_summarise_speed_errors_and_tokens() {
        let ok = |ttft: f64, gen_secs: f64, tokens: u64| Timing {
            status: Some(200),
            duration: Some(ttft + gen_secs),
            ttft: Some(ttft),
            prompt_tokens: 100,
            completion_tokens: tokens,
            decode: Some((gen_secs, tokens)),
            prefill: Some((0.5, 100)),
        };
        let items = vec![
            fin(1, "dm", "gpu:1", "ok", ok(1.0, 10.0, 200)),
            fin(2, "dm", "gpu:1", "ok", ok(3.0, 10.0, 400)),
            fin(3, "n8n", "gpu:1", "backend_error", Timing { duration: Some(2.0), ..Default::default() }),
            fin(4, "n8n", "cpu:2", "ok", Timing { status: Some(500), ..Default::default() }),
            fin(5, "n8n", "cpu:2", "client_gone", Timing::default()),
        ];
        let by_backend = group_by(&items, |f| f.backend.clone());
        let gpu = by_backend["gpu:1"].clone().summary();
        assert_eq!((gpu.requests, gpu.errors), (3, 1));
        assert_eq!((gpu.ttft_p50, gpu.ttft_p95), (Some(1.0), Some(3.0)));
        assert_eq!(gpu.gen_p50, Some(20.0));
        assert_eq!(gpu.prefill_tps, Some(200.0));
        assert_eq!(gpu.busy_secs, 26.0);
        // A 5xx counts as an error; a client hanging up does not.
        let cpu = by_backend["cpu:2"].clone().summary();
        assert_eq!((cpu.requests, cpu.errors, cpu.gen_p50), (2, 1, None));

        let by_client = group_by(&items, |f| Some(f.client.clone()));
        let dm = by_client["dm"].clone().summary();
        assert_eq!((dm.prompt_tokens, dm.completion_tokens, dm.wait_p95), (200, 600, Some(1.0)));
    }

    #[test]
    fn buckets_split_the_window_evenly() {
        let b = buckets(0, 30_000, [(0, 1), (999, 2), (1_000, 3), (29_999, 4), (30_000, 5)]);
        assert_eq!(b.len(), SPARK_BUCKETS);
        assert_eq!(b[0], vec![1, 2]);
        assert_eq!(b[1], vec![3]);
        assert_eq!(b[SPARK_BUCKETS - 1], vec![4]);
        assert_eq!(b.iter().map(Vec::len).sum::<usize>(), 4);
    }
}
