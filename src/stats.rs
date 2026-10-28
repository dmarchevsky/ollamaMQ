//! Rolling history behind the web UI's Overview page.
//!
//! [`crate::metrics`] gives Prometheus cumulative counters and histograms;
//! the Overview needs "the last 15 minutes" without a Prometheus server. This
//! keeps `settings.history_hours` of finished requests in memory: one record
//! per finished request.
//!
//! When [`Stats::attach_journal`] runs at startup, the same records are also
//! journalled as JSONL lines next to the conversation log and read back after
//! a restart — the Activity view keeps its curve across a restart, up to the
//! configured retention (size-rotated like the conversation log, so the
//! journal cannot grow without bound).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::lock::LockExt;

/// Default Activity retention in hours — what `history_hours` falls back to.
pub const DEFAULT_HISTORY_HOURS: u64 = 24;
/// Default cap on finished-request records — `history_max_records`' fallback.
pub const DEFAULT_MAX_RECORDS: usize = 50_000;
/// How far back the history goes by default (the longest Overview window).
pub const HISTORY: Duration = Duration::from_secs(DEFAULT_HISTORY_HOURS * 3600);
/// Points in each sparkline.
pub const SPARK_BUCKETS: usize = 30;

/// What was measured while serving one request. Parts the backend or the
/// response shape did not allow measuring stay `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
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
    /// Prefill seconds lost re-evaluating evicted context (see
    /// [`crate::dispatcher::estimate_cache_waste`]); `None` when unmeasurable.
    pub waste_secs: Option<f64>,
}

/// One request that left the proxy: served, failed, or refused.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub at_ms: u64,
    pub client: String,
    /// API key label (or id), `none` for keyless traffic — the per-key identity
    /// the Overview's client+label mode groups by.
    pub key: String,
    pub model: String,
    /// Metrics label of the backend; `None` when it was never dispatched.
    pub backend: Option<String>,
    /// The `outcome` label of `ollamamq_requests_total`.
    pub outcome: &'static str,
    /// Short tag of the conversation this request belongs to (see
    /// [`crate::dispatcher::SessionKey::tag`]); `None` for non-chat requests
    /// and sticky-less traffic.
    pub session: Option<String>,
    /// Human label the client sent for that conversation (`X-Session-Name`);
    /// display only, never part of the key.
    pub session_label: Option<String>,
    /// Scheduling level the request was served at: `normal`, `high` or `top`.
    pub priority: &'static str,
    /// Log correlation id of the request (see [`crate::dispatcher::LogEvent::rid`])
    /// so the Overview can jump to its log round; `None` for actions that were
    /// never proxied requests (model load/unload records).
    pub rid: Option<u64>,
    /// Tools this request's messages named (see [`crate::convlog::tool_names`]):
    /// what the client did between the previous turn and this request. The
    /// Overview names its idle line with these.
    pub tools: Vec<String>,
    /// Seconds it waited in the queue; `None` when refused before queuing.
    pub queue_wait: Option<f64>,
    /// The conversation's full sticky key, not just its display tag: what a
    /// linked compaction inherits (see [`crate::link`]). In-memory only — the
    /// log and the API keep carrying the tag.
    pub session_key: Option<crate::dispatcher::SessionKey>,
    /// What the request turned out to be, `Some("compaction")` for a client's
    /// summarization call (see [`crate::classify`]); `None` is ordinary traffic.
    pub kind: Option<&'static str>,
    /// How a compaction was attached to this conversation (`recency`,
    /// `client header`, …); `None` when it is no compaction, or one that stayed
    /// unlinked. Display provenance, shown in the UI's tooltip.
    pub link: Option<String>,
    /// Absolute stage times (unix ms) of this request, measured as they happened.
    /// The Overview draws its timeline from these; `None` halves (an old record,
    /// or a stage that never happened) fall back to duration maths.
    pub stage: crate::convlog::StageMs,
    pub timing: Timing,
}

/// The metric/UI label of a key: the label (or id) or `none` for keyless.
pub fn key_or_none(label: &Option<String>) -> String {
    label.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "none".to_string())
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

    /// The journal form of this record (see [`Stats::attach_journal`]).
    fn to_journal(&self) -> JournalLine {
        JournalLine {
            at_ms: self.at_ms,
            client: self.client.clone(),
            key: self.key.clone(),
            model: self.model.clone(),
            backend: self.backend.clone(),
            outcome: self.outcome.to_string(),
            session: self.session.clone(),
            session_label: self.session_label.clone(),
            priority: self.priority.to_string(),
            rid: self.rid,
            tools: self.tools.clone(),
            queue_wait: self.queue_wait,
            kind: self.kind.map(str::to_string),
            link: self.link.clone(),
            stage: self.stage.clone(),
            timing: self.timing,
        }
    }
}

/// One [`Finished`] as it sits in the Activity journal. `Finished` itself
/// cannot round-trip: `outcome`, `priority` and `kind` are `&'static` labels
/// in memory, and `session_key` only feeds compaction linking, whose window
/// is minutes wide — replayed records are outside it by construction.
#[derive(Serialize, Deserialize)]
struct JournalLine {
    at_ms: u64,
    client: String,
    key: String,
    model: String,
    backend: Option<String>,
    outcome: String,
    session: Option<String>,
    session_label: Option<String>,
    priority: String,
    rid: Option<u64>,
    tools: Vec<String>,
    queue_wait: Option<f64>,
    kind: Option<String>,
    link: Option<String>,
    stage: crate::convlog::StageMs,
    timing: Timing,
}

impl JournalLine {
    /// Rebuild the in-memory record. `outcome`/`priority`/`kind` become
    /// `&'static` by leaking the journal string: replay is bounded by
    /// `history_max_records` and runs once per process, so the total leak is
    /// a few MB at worst — cheaper than widening every caller of these
    /// `&'static` labels to owned strings.
    fn into_finished(self) -> Finished {
        let leak = |s: String| -> &'static str {
            if s.is_empty() { "" } else { Box::leak(s.into_boxed_str()) }
        };
        Finished {
            at_ms: self.at_ms,
            client: self.client,
            key: self.key,
            model: self.model,
            backend: self.backend,
            outcome: leak(self.outcome),
            session: self.session,
            session_label: self.session_label,
            priority: leak(self.priority),
            rid: self.rid,
            tools: self.tools,
            queue_wait: self.queue_wait,
            stage: self.stage,
            timing: self.timing,
            session_key: None,
            kind: self.kind.map(leak),
            link: self.link,
        }
    }
}

/// Journal size before it rotates (the chain drops the oldest slot, like the
/// conversation log).
const JOURNAL_MAX_BYTES: u64 = 10 << 20;
/// Rotated journal slots kept besides the live file.
const JOURNAL_MAX_FILES: usize = 2;

pub struct Stats {
    finished: Mutex<VecDeque<Finished>>,
    /// Per backend label: `(prefill secs, waste secs)` summed over every request
    /// that had a measurable cache waste (see [`Stats::record`]). Accumulated as
    /// requests finish rather than recomputed from `finished`, because a metrics
    /// scrape or a UI poll used to copy the whole 20k-record history and search
    /// it per backend on every call.
    waste: Mutex<HashMap<String, (f64, f64)>>,
    /// `settings.history_hours` as ms — the longest Overview window and the
    /// age records fall out at (`set_history`, read via `history_ms`).
    history_ms: std::sync::atomic::AtomicU64,
    /// `settings.history_max_records` — the count cap on finished records.
    max_records: std::sync::atomic::AtomicUsize,
    /// Journal writer handle, `Some` after [`Self::attach_journal`]. Each
    /// finished record is sent to the writer thread; the channel is unbounded
    /// by intent — a stalled disk must never stall a served request.
    journal: Mutex<Option<Sender<JournalLine>>>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            finished: Mutex::new(VecDeque::new()),
            waste: Mutex::new(HashMap::new()),
            history_ms: std::sync::atomic::AtomicU64::new(DEFAULT_HISTORY_HOURS * 3_600_000),
            max_records: std::sync::atomic::AtomicUsize::new(DEFAULT_MAX_RECORDS),
            journal: Mutex::new(None),
        }
    }
}

impl Stats {
    /// Apply `settings.history_hours` / `settings.history_max_records`. Live:
    /// a smaller value prunes lazily as records arrive, a larger one simply
    /// lets the queue grow before the next prune round.
    pub fn set_history(&self, hours: u64, max_records: usize) {
        self.history_ms
            .store(hours.max(1) * 3_600_000, std::sync::atomic::Ordering::Relaxed);
        self.max_records
            .store(max_records.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// The configured Activity retention, in whole hours.
    pub fn history_hours(&self) -> u64 {
        self.history_ms.load(std::sync::atomic::Ordering::Relaxed) / 3_600_000
    }

    /// The configured Activity retention, in ms.
    pub fn history_ms(&self) -> u64 {
        self.history_ms.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn max_records(&self) -> usize {
        self.max_records.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Read the journal back into memory and start its writer thread.
    /// Callers that fail to attach simply keep the history in memory only.
    pub fn attach_journal(&self, path: String) -> Result<(), String> {
        self.replay_journal(&path);
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("activity-journal".into())
            .spawn(move || journal_writer(&path, rx))
            .map_err(|e| format!("cannot start the journal writer: {e}"))?;
        *self.journal.lock_or_recover() = Some(tx);
        Ok(())
    }

    /// Load the journal chain (oldest slot first) into the queue, dropping
    /// records outside the retention window and past the count cap. Intended
    /// to run before traffic arrives — the queue order beyond that is the
    /// best-effort one of the load order.
    fn replay_journal(&self, path: &str) {
        let cutoff = now_ms().saturating_sub(self.history_ms());
        let mut loaded: VecDeque<Finished> = VecDeque::new();
        let mut files: Vec<String> = (1..=JOURNAL_MAX_FILES)
            .rev()
            .map(|i| format!("{}.{i}", path))
            .collect();
        files.push(path.to_string());
        for file in files {
            let Ok(data) = std::fs::read(&file) else { continue };
            for line in String::from_utf8_lossy(&data).lines() {
                let Ok(rec) = serde_json::from_str::<JournalLine>(line.trim()) else {
                    continue; // torn tail or a newer schema — skip
                };
                if rec.at_ms < cutoff {
                    continue;
                }
                loaded.push_back(rec.into_finished());
            }
        }
        let max = self.max_records();
        while loaded.len() > max {
            loaded.pop_front();
        }
        let mut q = self.finished.lock_or_recover();
        q.extend(loaded);
    }
}
impl Stats {
    pub fn record(&self, f: Finished) {
        if let Some(waste_secs) = f.timing.waste_secs
            && let Some(backend) = f.backend.as_deref()
        {
            // Same rule the dashboard's Waste column always used: a turn only
            // counts when its waste was measurable, and its prefill time
            // joins the denominator at that moment.
            let prefill = f.timing.prefill.map(|(secs, _)| secs).unwrap_or(0.0);
            let mut waste = self.waste.lock_or_recover();
            let acc = waste.entry(backend.to_string()).or_insert((0.0, 0.0));
            acc.0 += prefill;
            acc.1 += waste_secs;
        }
        // Snapshot for the journal before `f` is moved into the queue.
        let line = self
            .journal
            .lock_or_recover()
            .is_some()
            .then(|| f.to_journal());
        let cutoff = f.at_ms.saturating_sub(self.history_ms());
        let max = self.max_records();
        let mut q = self.finished.lock_or_recover();
        while q.front().is_some_and(|x| x.at_ms < cutoff) || q.len() >= max {
            q.pop_front();
        }
        q.push_back(f);
        if let Some(line) = line {
            if let Some(tx) = self.journal.lock_or_recover().as_ref() {
                let _ = tx.send(line);
            }
        }
    }

    /// Lifetime `(prefill secs, waste secs)` remembered for one backend label,
    /// `(0.0, 0.0)` where nothing was measurable.
    pub fn backend_waste(&self, backend_label: &str) -> (f64, f64) {
        self.waste
            .lock_or_recover()
            .get(backend_label)
            .copied()
            .unwrap_or((0.0, 0.0))
    }

    /// Requests finished at or after `from_ms`, oldest first.
    pub fn finished_since(&self, from_ms: u64) -> Vec<Finished> {
        let q = self.finished.lock_or_recover();
        let start = q.partition_point(|f| f.at_ms < from_ms);
        q.range(start..).cloned().collect()
    }

    /// The finished requests that could parent a compaction from this client
    /// (see [`crate::link`]): same client, same API key, same model (so the
    /// parent's affinity key is the right one to inherit), inside the linking
    /// window, and not themselves compactions. Collected here rather than by
    /// cloning the window because the request path calls it on every summary.
    pub fn compaction_candidates(
        &self,
        client: &str,
        key: &str,
        model: &str,
        self_conversation: Option<&str>,
        named_tag: Option<&str>,
    ) -> Vec<crate::link::Candidate> {
        let cutoff = now_ms().saturating_sub(crate::link::WINDOW_MS);
        let q = self.finished.lock_or_recover();
        q.iter()
            .rev()
            .take_while(|f| f.at_ms >= cutoff)
            .filter(|f| {
                f.client == client
                    && f.key == key
                    && f.model == model
                    && f.kind != Some(crate::classify::KIND_COMPACTION)
            })
            .filter_map(|f| {
                let k = f.session_key.as_ref()?;
                Some(crate::link::Candidate {
                    key: k.clone(),
                    priority: f.priority,
                    label: f.session_label.clone(),
                    at_ms: f.at_ms,
                    is_self: self_conversation.is_some_and(|c| *c == k.conversation),
                    named: named_tag.is_some_and(|t| t == k.tag()),
                })
            })
            .collect()
    }

    /// The `kind` of the newest finished record of one conversation, if any:
    /// [`crate::classify::KIND_COMPACTION`] means this conversation has just
    /// resumed after a summary. Bounded scan — the ring is hours of traffic and
    /// only the newest turn of one conversation matters.
    pub fn last_kind(&self, key: &crate::dispatcher::SessionKey) -> Option<&'static str> {
        self.finished
            .lock_or_recover()
            .iter()
            .rev()
            .take(512)
            .find(|f| f.session_key.as_ref().is_some_and(|k| k == key))
            .and_then(|f| f.kind)
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The journal writer thread: append one line per finished record, flush per
/// batch, and rotate the chain at the size cap — same shape as the
/// conversation log's writer, so a slow disk costs a thread, never a request.
fn journal_writer(path: &str, rx: Receiver<JournalLine>) {
    let open = |path: &str| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    };
    let mut file = open(path);
    let mut size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    loop {
        // Block for the next record, then drain everything queued behind it:
        // one wake and one flush per burst, not one per record.
        let Ok(first) = rx.recv() else { break };
        let mut burst = vec![first];
        burst.extend(rx.try_iter());
        for rec in burst {
            let Ok(mut data) = serde_json::to_string(&rec) else {
                continue;
            };
            data.push('\n');
            if size + data.len() as u64 > JOURNAL_MAX_BYTES {
                drop(file.take());
                rotate_journal(path);
                size = 0;
                file = open(path);
            }
            if let Some(f) = &mut file
                && let Err(e) = f.write_all(data.as_bytes())
            {
                tracing::warn!("activity journal: cannot append to {path}: {e}");
                file = None;
            } else {
                size += data.len() as u64;
            }
        }
        if let Some(f) = &mut file {
            let _ = f.flush();
        }
    }
    if let Some(mut f) = file {
        let _ = f.flush();
    }
}

/// Shift the journal chain like the conversation log's rotation: the oldest
/// slot (`.{JOURNAL_MAX_FILES}`) falls off, every slot shifts one older, and
/// the live file becomes `.1` — the same order [`Stats::replay_journal`] reads
/// (oldest slot first, live last). A failed rename leaves the live file alone;
/// the writer keeps appending to it and the age filter on replay still bounds
/// the history.
fn rotate_journal(path: &str) {
    let _ = std::fs::remove_file(format!("{}.{JOURNAL_MAX_FILES}", path));
    for i in (1..JOURNAL_MAX_FILES).rev() {
        let _ = std::fs::rename(format!("{}.{i}", path), format!("{}.{}", path, i + 1));
    }
    let _ = std::fs::rename(path, format!("{path}.1"));
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
    waste_secs: f64,
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
    /// Prefill seconds lost to cache eviction in the window.
    pub waste_secs: f64,
    /// Share of the window's prefill time lost to eviction, percent 0–100;
    /// `None` when no prefill was measurable (shown as "—", not 0).
    pub waste_pct: Option<u8>,
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
        self.waste_secs += t.waste_secs.unwrap_or(0.0);
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
            waste_secs: self.waste_secs,
            waste_pct: (self.prefill_secs > 0.0)
                .then(|| ((self.waste_secs * 100.0 / self.prefill_secs).round() as u8).min(100)),
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
            key: "none".into(),
            model: "llama3".into(),
            backend: Some(backend.into()),
            outcome,
            session: None,
            session_label: None,
            priority: "normal",
            rid: None,
            tools: vec![],
            queue_wait: Some(1.0),
            stage: Default::default(),
            timing,
            session_key: None,
            kind: None,
            link: None,
        }
    }

    /// The same record carrying a conversation, for the compaction-linking tests.
    fn sess(
        at_ms: u64,
        conversation: &str,
        priority: &'static str,
        kind: Option<&'static str>,
    ) -> Finished {
        Finished {
            at_ms,
            client: "pi".into(),
            key: "main".into(),
            model: "qwen".into(),
            backend: Some("gpu1".into()),
            outcome: "ok",
            session: Some("t".into()),
            session_label: None,
            priority,
            rid: None,
            tools: vec![],
            queue_wait: Some(1.0),
            stage: Default::default(),
            timing: Default::default(),
            session_key: Some(crate::dispatcher::SessionKey {
                conversation: conversation.into(),
                model: "qwen".into(),
            }),
            kind,
            link: None,
        }
    }

    #[test]
    fn compaction_candidates_keep_only_the_likely_parents() {
        let s = Stats::default();
        let now = now_ms();
        // Recorded oldest first, like the ring really is: the scan stops at the
        // first record past the window, so a stale record has to come first.
        s.record(sess(
            now - crate::link::WINDOW_MS - 1,
            "stale",
            "normal",
            None,
        ));
        s.record(sess(now - 1_000, "thread-a", "normal", None));
        s.record(Finished {
            client: "other".into(),
            ..sess(now - 500, "other-client", "normal", None)
        });
        s.record(Finished {
            key: "second-key".into(),
            ..sess(now - 500, "other-key", "normal", None)
        });
        s.record(Finished {
            model: "gpt".into(),
            ..sess(now - 500, "other-model", "normal", None)
        });
        s.record(sess(
            now - 400,
            "already-a-summary",
            "normal",
            Some(crate::classify::KIND_COMPACTION),
        ));
        let c = s.compaction_candidates("pi", "main", "qwen", None, None);
        let convs: Vec<&str> = c.iter().map(|x| x.key.conversation.as_str()).collect();
        assert_eq!(convs, vec!["thread-a"]);
        // The summary request's own fingerprinted conversation is marked so it
        // cannot be attached to itself.
        let c = s.compaction_candidates("pi", "main", "qwen", Some("thread-a"), None);
        assert!(c[0].is_self);
        // A named tag is matched on the parent's own tag, so a client can name
        // the conversation the UI shows it by.
        let tag = crate::dispatcher::SessionKey {
            conversation: "thread-a".into(),
            model: "qwen".into(),
        }
        .tag();
        let c = s.compaction_candidates("pi", "main", "qwen", None, Some(&tag));
        assert!(c[0].named);
        let c = s.compaction_candidates("pi", "main", "qwen", None, Some("deadbeef"));
        assert!(!c.iter().any(|x| x.named));
    }

    #[test]
    fn last_kind_names_the_boundary_after_a_summary() {
        let s = Stats::default();
        let now = now_ms();
        let a = crate::dispatcher::SessionKey {
            conversation: "thread-a".into(),
            model: "qwen".into(),
        };
        let b = crate::dispatcher::SessionKey {
            conversation: "thread-b".into(),
            model: "qwen".into(),
        };
        s.record(sess(now - 3_000, "thread-a", "normal", None));
        assert_eq!(s.last_kind(&a), None);
        s.record(sess(
            now - 2_000,
            "thread-a",
            "normal",
            Some(crate::classify::KIND_COMPACTION),
        ));
        assert_eq!(
            s.last_kind(&a),
            Some(crate::classify::KIND_COMPACTION)
        );
        // Another conversation's summary says nothing about this one…
        assert_eq!(s.last_kind(&b), None);
        // …and the thread resuming ends the boundary.
        s.record(sess(now - 1_000, "thread-a", "normal", None));
        assert_eq!(s.last_kind(&a), None);
        assert_eq!(s.last_kind(&b), None);
    }

    #[test]
    fn waste_is_tallied_per_backend_as_requests_finish() {
        let s = Stats::default();
        // A turn with measurable waste carries its prefill into the denominator.
        s.record(fin(
            1_000,
            "a",
            "gpu1",
            "ok",
            Timing {
                prefill: Some((30.0, 1000)),
                waste_secs: Some(6.0),
                ..Default::default()
            },
        ));
        // A turn without waste adds nothing — not even its prefill.
        s.record(fin(
            1_100,
            "a",
            "gpu1",
            "ok",
            Timing {
                prefill: Some((20.0, 900)),
                ..Default::default()
            },
        ));
        s.record(fin(
            1_200,
            "a",
            "gpu1",
            "ok",
            Timing {
                prefill: Some((10.0, 500)),
                waste_secs: Some(1.5),
                ..Default::default()
            },
        ));
        s.record(fin(
            1_300,
            "b",
            "gpu2",
            "ok",
            Timing {
                prefill: Some((5.0, 100)),
                waste_secs: Some(0.5),
                ..Default::default()
            },
        ));
        assert_eq!(s.backend_waste("gpu1"), (40.0, 7.5));
        assert_eq!(s.backend_waste("gpu2"), (5.0, 0.5));
        assert_eq!(s.backend_waste("never-seen"), (0.0, 0.0));
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
    fn groups_summarise_speed_errors_and_tokens() {
        let ok = |ttft: f64, gen_secs: f64, tokens: u64| Timing {
            status: Some(200),
            duration: Some(ttft + gen_secs),
            ttft: Some(ttft),
            prompt_tokens: 100,
            completion_tokens: tokens,
            decode: Some((gen_secs, tokens)),
            prefill: Some((0.5, 100)),
            waste_secs: None,
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

    fn tmpj(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("ollamamq_journal_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        for i in 0..=JOURNAL_MAX_FILES {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}.{}", path.display(), i));
        }
        path.to_string_lossy().into_owned()
    }

    fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    fn line_at_ms(path: &str) -> Vec<u64> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<JournalLine>(l.trim()).ok())
            .map(|r| r.at_ms)
            .collect()
    }

    /// A restart is the whole point: what the writer journalled, the next
    /// process replays — fields intact, order preserved.
    #[test]
    fn journal_roundtrips_records_through_a_restart() {
        let path = tmpj("roundtrip.jsonl");
        let now = now_ms();
        {
            let s1 = Stats::default();
            s1.attach_journal(path.clone()).expect("attach");
            s1.record(fin(
                now - 2_000,
                "ada",
                "gpu1",
                "ok",
                Timing {
                    status: Some(200),
                    ttft: Some(1.5),
                    prompt_tokens: 7,
                    completion_tokens: 9,
                    ..Default::default()
                },
            ));
            s1.record(fin(now - 1_000, "dm", "gpu1", "backend_error", Timing::default()));
            assert!(
                wait_until(|| line_at_ms(&path).len() == 2),
                "the writer journalled both records"
            );
        } // sender dropped here; the writer thread exits on disconnect
        let s2 = Stats::default();
        s2.attach_journal(path).expect("re-attach");
        let all = s2.finished_since(0);
        assert_eq!(all.iter().map(|f| f.at_ms).collect::<Vec<_>>(), vec![now - 2_000, now - 1_000]);
        assert_eq!((all[0].client.as_str(), all[0].outcome, all[0].timing.ttft), ("ada", "ok", Some(1.5)));
        assert_eq!((all[1].client.as_str(), all[1].outcome), ("dm", "backend_error"));
    }

    /// Replay is the retention boundary: records older than the configured
    /// window do not come back, and a torn tail line is skipped, not fatal.
    #[test]
    fn replay_honours_the_window_and_skips_torn_lines() {
        let path = tmpj("window.jsonl");
        let now = now_ms();
        let rec = |at_ms: u64| {
            serde_json::to_string(&fin(at_ms, "a", "b", "ok", Timing::default()).to_journal())
                .unwrap()
        };
        std::fs::write(
            &path,
            format!("{}\n{}\n{{trunc\n", rec(now - 30 * 3_600_000), rec(now - 60_000)),
        )
        .unwrap();
        let s = Stats::default(); // 24 h window
        s.attach_journal(path).expect("attach");
        assert_eq!(
            s.finished_since(0).iter().map(|f| f.at_ms).collect::<Vec<_>>(),
            vec![now - 60_000],
            "only the record inside the window replays, the torn line is skipped"
        );
    }

    /// The count cap bounds replay too — a journal far past `max_records`
    /// keeps its newest tail.
    #[test]
    fn replay_caps_at_history_max_records() {
        let path = tmpj("cap.jsonl");
        let now = now_ms();
        let body: String = (0..5)
            .map(|i| {
                let j = fin(now - 5 + i, "a", "b", "ok", Timing::default()).to_journal();
                format!("{}\n", serde_json::to_string(&j).unwrap())
            })
            .collect();
        std::fs::write(&path, body).unwrap();
        let s = Stats::default();
        s.set_history(24, 2);
        s.attach_journal(path).expect("attach");
        assert_eq!(s.finished_since(0).len(), 2);
        assert_eq!(s.finished_since(0)[0].at_ms, now - 2, "the newest tail survives");
    }
}
