//! LLM review of conversations the operator sends from the web UI.
//!
//! A review hands one conversation, as text (see [`transcript`]), to a model
//! with a prompt from the library (see [`store::Prompts`]) or one written for
//! that review, and keeps what the model says.
//!
//! Reviews are the lowest-priority work the proxy does, and they never cost a
//! user anything:
//!
//! - One starts only on a backend that runs no user request, has none queued
//!   that it could serve, and has been free of user requests for
//!   `review.quiet_secs` — so a review does not slip into the seconds between
//!   an agent's turns.
//! - A review loads its model only where that model is already resident or
//!   nothing is, unless the backend has been quiet for `review.swap_idle_secs`:
//!   evicting a user's model costs that user a reload.
//! - A running review is not counted against the backend's capacity, so a user
//!   request that arrives meanwhile is dispatched at once, not held behind it.
//!   The review finishes alongside; no new one starts there until the backend
//!   is quiet again.
//!
//! The operator can override all of that with **Run now**: the review then
//! starts on the first online backend that has its model and is not loading or
//! unloading one, beside whatever users are doing there.
//!
//! The worker does not go through the user queue: reviews have their own, and
//! their calls are not written to the conversation log or the metrics.

pub mod store;
pub mod transcript;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use serde_json::{Value, json};
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::clients::now_ms;
use crate::config::ReviewSettings;
use crate::dispatcher::{ApiFamily, AppState, BackendStatus};
use crate::lock::LockExt;
pub use store::{Job, Prompt, Status};

/// How often the worker looks again without being woken. The quiet windows it
/// waits for are minutes long; this only bounds how late a countdown ends.
const TICK: std::time::Duration = std::time::Duration::from_secs(5);

/// Reviews kept; past it the oldest finished ones go first.
const MAX_JOBS: usize = 2000;

pub struct Review {
    jobs: Mutex<BTreeMap<String, Job>>,
    dir: Mutex<Option<PathBuf>>,
    prompts: Mutex<store::Prompts>,
    cfg: Mutex<ReviewSettings>,
    /// Running reviews' tasks, so Cancel can stop one.
    aborts: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    wake: Notify,
    started: AtomicBool,
    seq: AtomicU64,
}


impl Default for Review {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a queued review is not running on one backend. Ordered from "about to
/// start" to "cannot start here at all": the UI shows the nearest one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Wait {
    /// Quiet for this many more seconds and it starts.
    Quiet(u64),
    /// Quiet for this many more seconds and it may load its model here.
    Swap(u64),
    ReviewRunning,
    Busy,
    UserQueued,
    ControlOp,
    Offline,
    NoModel,
}

impl Wait {
    fn describe(&self, model: &str) -> String {
        let mmss = |s: u64| format!("{}:{:02}", s / 60, s % 60);
        match self {
            Wait::Quiet(s) => format!("waiting for {} more of quiet", mmss(*s)),
            Wait::Swap(s) => format!("another model is loaded — may swap to {model} in {}", mmss(*s)),
            Wait::ReviewRunning => "another review is running on the backend".into(),
            Wait::Busy => "the backend is serving users".into(),
            Wait::UserQueued => "users are waiting for the backend".into(),
            Wait::ControlOp => "a model is being loaded or unloaded".into(),
            Wait::Offline => "the backend is offline".into(),
            Wait::NoModel => format!("no backend has {model}"),
        }
    }
}

/// May a review of `model` start on `b` now? `user_waiting`: a queued user
/// request could be served by `b`. `force` (Run now) skips everything but
/// "the backend cannot run it at all".
pub fn eligible(
    b: &BackendStatus,
    now: u64,
    cfg: &ReviewSettings,
    model: &str,
    control_busy: bool,
    user_waiting: bool,
    force: bool,
) -> Result<(), Wait> {
    if b.disabled || !b.is_online {
        return Err(Wait::Offline);
    }
    let chat = b.api_type.is_empty()
        || b.api_type.contains(ApiFamily::Ollama)
        || b.api_type.contains(ApiFamily::OpenAi);
    if !chat || !crate::dispatcher::model_routable(model, &b.available_models) {
        return Err(Wait::NoModel);
    }
    if control_busy {
        return Err(Wait::ControlOp);
    }
    if force {
        return Ok(());
    }
    if b.review_running {
        return Err(Wait::ReviewRunning);
    }
    if b.active_requests > 0 {
        return Err(Wait::Busy);
    }
    if user_waiting {
        return Err(Wait::UserQueued);
    }
    let quiet = now.saturating_sub(b.last_user_ms) / 1000;
    if quiet < cfg.quiet_secs {
        return Err(Wait::Quiet(cfg.quiet_secs - quiet));
    }
    let resident = !b.loaded_state_known
        || b.loaded_models.is_empty()
        || crate::dispatcher::model_loaded_on(b, model);
    if !resident && quiet < cfg.swap_idle_secs {
        return Err(Wait::Swap(cfg.swap_idle_secs - quiet));
    }
    Ok(())
}

/// A failed submit: the HTTP status it maps to and why.
#[derive(Debug)]
pub enum SubmitError {
    NotFound(String),
    Invalid(String),
}

/// What the operator sends.
pub struct Request<'a> {
    pub key: &'a str,
    pub prompt_id: Option<&'a str>,
    /// Overrides the library prompt's text for this review only.
    pub prompt: Option<&'a str>,
    pub model: Option<&'a str>,
    /// Start at once (see [`Review::run_now`]).
    pub run_now: bool,
}

impl Review {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(BTreeMap::new()),
            dir: Mutex::new(None),
            prompts: Mutex::new(store::Prompts::in_memory()),
            cfg: Mutex::new(ReviewSettings::default()),
            aborts: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            started: AtomicBool::new(false),
            seq: AtomicU64::new(0),
        }
    }

    /// Give the reviews a home on disk: past jobs are loaded from `dir`, the
    /// prompt library from `prompts`. Without them everything stays in memory.
    pub fn attach(&self, dir: Option<PathBuf>, prompts: Option<PathBuf>) {
        if let Some(d) = &dir {
            match std::fs::create_dir_all(d) {
                Ok(()) => {
                    let loaded = store::load_jobs(d);
                    let mut jobs = self.jobs.lock_or_recover();
                    for j in loaded {
                        jobs.insert(j.id.clone(), j);
                    }
                    info!("review: {} review(s) in {}", jobs.len(), d.display());
                }
                Err(e) => warn!("review: cannot create {}: {e}; reviews stay in memory", d.display()),
            }
        }
        *self.dir.lock_or_recover() = dir;
        if let Some(p) = prompts {
            *self.prompts.lock_or_recover() = store::Prompts::load(p);
        }
    }

    pub fn apply(&self, s: ReviewSettings) {
        *self.cfg.lock_or_recover() = s;
        self.wake.notify_one();
    }

    pub fn settings(&self) -> ReviewSettings {
        self.cfg.lock_or_recover().clone()
    }

    fn persist(&self, job: &Job) {
        if let Some(d) = self.dir.lock_or_recover().clone() {
            store::save_job(&d, job);
        }
    }

    /// Change one job and write it back. Returns the changed copy.
    fn update(&self, id: &str, f: impl FnOnce(&mut Job)) -> Option<Job> {
        let job = {
            let mut jobs = self.jobs.lock_or_recover();
            let j = jobs.get_mut(id)?;
            f(j);
            j.clone()
        };
        self.persist(&job);
        Some(job)
    }

    /// Every review, newest first; `conv` keeps one conversation's.
    pub fn list(&self, conv: Option<&str>) -> Vec<Value> {
        let jobs = self.jobs.lock_or_recover();
        let mut out: Vec<&Job> = jobs.values().filter(|j| conv.is_none_or(|k| j.conv_key == k)).collect();
        out.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then(b.id.cmp(&a.id)));
        out.into_iter().map(Job::summary).collect()
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        self.jobs.lock_or_recover().get(id).cloned()
    }

    /// Queued and running reviews, for the nav count.
    pub fn active(&self) -> usize {
        self.jobs
            .lock_or_recover()
            .values()
            .filter(|j| matches!(j.status, Status::Queued | Status::Running))
            .count()
    }

    pub fn prompts(&self) -> Value {
        let p = self.prompts.lock_or_recover();
        let cfg = self.settings();
        json!({
            "prompts": p.file.prompts,
            "default": p.default_prompt().id,
            "model": cfg.model,
            "enabled": cfg.enabled,
            "quiet_secs": cfg.quiet_secs,
        })
    }

    pub fn upsert_prompt(&self, id: Option<&str>, name: &str, text: &str, make_default: bool) -> Result<String, String> {
        self.prompts.lock_or_recover().upsert(id, name, text, make_default)
    }

    pub fn remove_prompt(&self, id: &str) -> Result<(), String> {
        self.prompts.lock_or_recover().remove(id)
    }

    /// Queue a review of the conversation `req.key` as it stands now.
    pub fn submit(&self, state: &AppState, req: Request<'_>) -> Result<Job, SubmitError> {
        let find = crate::convlog::Find { key: Some(req.key), ..Default::default() };
        let conv = state
            .convlog
            .index(&find)
            .ok_or_else(|| SubmitError::NotFound("no such conversation in the log".into()))?;
        let (first, last) = match (conv.turns.first(), conv.turns.last()) {
            (Some(f), Some(l)) => (f.rid, l.rid),
            _ => return Err(SubmitError::Invalid("the conversation has no turns".into())),
        };
        if conv.turns.iter().all(|t| t.redacted) {
            return Err(SubmitError::Invalid(
                "this conversation was recorded without its text (redacted): there is nothing to review".into(),
            ));
        }
        let cfg = self.settings();
        let model = req.model.map(str::trim).filter(|m| !m.is_empty()).unwrap_or(&cfg.model).to_string();
        if model.is_empty() {
            return Err(SubmitError::Invalid("no model: name one, or set the default review model in Settings".into()));
        }
        let (prompt_id, prompt_name, library_text) = {
            let p = self.prompts.lock_or_recover();
            let chosen = match req.prompt_id {
                Some(id) => p.get(id).ok_or_else(|| SubmitError::Invalid(format!("no prompt '{id}'")))?,
                None => p.default_prompt(),
            };
            (chosen.id.clone(), chosen.name.clone(), chosen.text.clone())
        };
        let text = req.prompt.map(str::trim).filter(|t| !t.is_empty());
        let custom = text.is_some_and(|t| t != library_text.trim());
        let prompt_text = text.map(str::to_string).unwrap_or(library_text);
        if prompt_text.chars().count() > 32_000 {
            return Err(SubmitError::Invalid("the prompt is longer than 32000 characters".into()));
        }
        let now = now_ms();
        let job = Job {
            id: format!("{:x}-{}", now, self.seq.fetch_add(1, Relaxed) % 1000),
            conv_key: conv.key.clone(),
            user: conv.user.clone(),
            label: conv.session_label.clone(),
            rid_from: first,
            rid_to: last,
            turns: conv.turns.len(),
            prompt_id: (!custom).then_some(prompt_id),
            prompt_name: if custom { format!("{prompt_name} (edited)") } else { prompt_name },
            prompt_text,
            custom,
            model,
            status: Status::Queued,
            wait_reason: None,
            backend: None,
            created_ms: now,
            started_ms: None,
            finished_ms: None,
            input_chars: None,
            truncated: false,
            result: None,
            error: None,
            prompt_tokens: None,
            completion_tokens: None,
            attempts: 0,
            run_now: req.run_now,
        };
        self.persist(&job);
        {
            let mut jobs = self.jobs.lock_or_recover();
            jobs.insert(job.id.clone(), job.clone());
            self.trim(&mut jobs);
        }
        info!("review: {} queued for {} on {}", job.id, job.conv_key, job.model);
        self.wake.notify_one();
        Ok(job)
    }

    /// Past [`MAX_JOBS`], drop the oldest finished reviews.
    fn trim(&self, jobs: &mut BTreeMap<String, Job>) {
        if jobs.len() <= MAX_JOBS {
            return;
        }
        let mut done: Vec<(u64, String)> = jobs
            .values()
            .filter(|j| !matches!(j.status, Status::Queued | Status::Running))
            .map(|j| (j.created_ms, j.id.clone()))
            .collect();
        done.sort();
        let dir = self.dir.lock_or_recover().clone();
        for (_, id) in done.into_iter().take(jobs.len() - MAX_JOBS) {
            jobs.remove(&id);
            if let Some(d) = &dir {
                store::delete_job(d, &id);
            }
        }
    }

    /// Run a review again: a failed or cancelled one is queued as it was; a
    /// finished one is sent again as a new review of the conversation as it is
    /// now, so the old answer stays to compare.
    pub fn retry(&self, state: &AppState, id: &str) -> Result<Job, SubmitError> {
        let job = self.get(id).ok_or_else(|| SubmitError::NotFound("no such review".into()))?;
        match job.status {
            Status::Failed | Status::Cancelled => {
                let j = self
                    .update(id, |j| {
                        j.status = Status::Queued;
                        j.error = None;
                        j.started_ms = None;
                        j.finished_ms = None;
                        j.backend = None;
                        j.run_now = false;
                    })
                    .ok_or_else(|| SubmitError::NotFound("no such review".into()))?;
                self.wake.notify_one();
                Ok(j)
            }
            Status::Done => self.submit(
                state,
                Request {
                    key: &job.conv_key,
                    prompt_id: job.prompt_id.as_deref(),
                    prompt: Some(&job.prompt_text),
                    model: Some(&job.model),
                    run_now: false,
                },
            ),
            Status::Queued | Status::Running => Err(SubmitError::Invalid("the review has not finished".into())),
        }
    }

    /// Stop a review: a queued one is not started, a running one is cut off.
    pub fn cancel(&self, id: &str) -> Result<Job, SubmitError> {
        let job = self.get(id).ok_or_else(|| SubmitError::NotFound("no such review".into()))?;
        if !matches!(job.status, Status::Queued | Status::Running) {
            return Err(SubmitError::Invalid("the review has already finished".into()));
        }
        if let Some(h) = self.aborts.lock_or_recover().remove(id) {
            h.abort();
        }
        self.update(id, |j| {
            j.status = Status::Cancelled;
            j.finished_ms = Some(now_ms());
            j.wait_reason = None;
        })
        .ok_or_else(|| SubmitError::NotFound("no such review".into()))
    }

    /// Start a queued review now: it skips the quiet time, the swap wait, busy
    /// backends, the concurrency limit and the off switch. It still needs an
    /// online backend with its model; without one it stays queued, says why,
    /// and starts as soon as one turns up.
    pub fn run_now(&self, id: &str) -> Result<Job, SubmitError> {
        let job = self.get(id).ok_or_else(|| SubmitError::NotFound("no such review".into()))?;
        if job.status != Status::Queued {
            return Err(SubmitError::Invalid("only a queued review can be started".into()));
        }
        let j = self
            .update(id, |j| j.run_now = true)
            .ok_or_else(|| SubmitError::NotFound("no such review".into()))?;
        info!("review: {id} run now");
        self.wake.notify_one();
        Ok(j)
    }

    pub fn delete(&self, id: &str) -> bool {
        if let Some(h) = self.aborts.lock_or_recover().remove(id) {
            h.abort();
        }
        let gone = self.jobs.lock_or_recover().remove(id).is_some();
        if gone && let Some(d) = self.dir.lock_or_recover().clone() {
            store::delete_job(&d, id);
        }
        gone
    }
}

/// Start the review worker (once).
pub fn start(state: &Arc<AppState>) {
    if state.review.started.swap(true, Relaxed) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        loop {
            tick(&state);
            tokio::select! {
                _ = state.review.wake.notified() => {},
                _ = tokio::time::sleep(TICK) => {},
            }
        }
    });
}

/// Start what can start; say why the rest waits.
fn tick(state: &Arc<AppState>) {
    let review = &state.review;
    let cfg = review.settings();
    // Run-now reviews first (`!run_now` sorts them ahead), then oldest first.
    let mut queued: Vec<(bool, u64, String, String)> = {
        let jobs = review.jobs.lock_or_recover();
        jobs.values()
            .filter(|j| j.status == Status::Queued)
            .map(|j| (!j.run_now, j.created_ms, j.id.clone(), j.model.clone()))
            .collect()
    };
    if queued.is_empty() {
        return;
    }
    queued.sort();
    if !cfg.enabled {
        let (forced, held): (Vec<_>, Vec<_>) = queued.into_iter().partition(|q| !q.0);
        set_reasons(review, held.into_iter().map(|q| (q.2, "reviews are switched off in Settings".into())));
        if forced.is_empty() {
            return;
        }
        queued = forced;
    }
    let mut running = review.aborts.lock_or_recover().len();
    // Snapshots of what users are doing, taken one lock at a time. A request
    // that arrives between them and the claim below is no harm: a review never
    // holds a user request back, it only shares the backend with it.
    let waiting_models: Vec<Option<String>> = state
        .queues
        .lock_or_recover()
        .values()
        .flat_map(|q| q.iter().map(|t| t.requested_model.clone()))
        .collect();
    let control: HashSet<usize> = state.control_ops.lock_or_recover().keys().copied().collect();
    let mut reasons = Vec::new();
    for (later, _, id, model) in queued {
        let force = !later;
        if !force && running >= cfg.concurrency as usize {
            reasons.push((id, "waiting for a running review to finish".into()));
            continue;
        }
        match claim(state, &cfg, &model, &waiting_models, &control, force) {
            Ok((idx, label)) => {
                running += 1;
                launch(state, id, idx, label);
            }
            Err(w) => reasons.push((id, w.describe(&model))),
        }
    }
    set_reasons(review, reasons.into_iter());
}

fn set_reasons(review: &Review, reasons: impl Iterator<Item = (String, String)>) {
    let mut jobs = review.jobs.lock_or_recover();
    for (id, why) in reasons {
        if let Some(j) = jobs.get_mut(&id) {
            j.wait_reason = Some(why);
        }
    }
}

/// Find a backend for a review of `model` and mark it as running one. A
/// backend with the model already loaded is preferred.
fn claim(
    state: &AppState,
    cfg: &ReviewSettings,
    model: &str,
    waiting_models: &[Option<String>],
    control: &HashSet<usize>,
    force: bool,
) -> Result<(usize, String), Wait> {
    let now = now_ms();
    let mut backends = state.backends.lock_or_recover();
    let mut best: Option<Wait> = None;
    let mut pick: Option<usize> = None;
    for (idx, b) in backends.iter().enumerate() {
        let label = crate::metrics::backend_label(&b.url);
        if !cfg.backend.is_empty()
            && cfg.backend != b.url
            && Some(cfg.backend.as_str()) != b.name.as_deref()
            && cfg.backend != label
        {
            continue;
        }
        let user_waiting = waiting_models
            .iter()
            .any(|m| m.as_deref().is_none_or(|m| crate::dispatcher::model_routable(m, &b.available_models)));
        match eligible(b, now, cfg, model, control.contains(&idx), user_waiting, force) {
            Ok(()) => {
                let loaded = crate::dispatcher::model_loaded_on(b, model);
                // Forced: a backend without a review of its own beats one with.
                let better = pick.is_none_or(|p| force && backends[p].review_running && !b.review_running);
                if better || loaded {
                    pick = Some(idx);
                    if loaded {
                        break;
                    }
                }
            }
            Err(w) => {
                if best.as_ref().is_none_or(|b| w < *b) {
                    best = Some(w);
                }
            }
        }
    }
    match pick {
        Some(idx) => {
            backends[idx].review_running = true;
            let b = &backends[idx];
            Ok((idx, b.name.clone().unwrap_or_else(|| crate::metrics::backend_label(&b.url))))
        }
        None => Err(best.unwrap_or(Wait::NoModel)),
    }
}

/// Clears the backend's review mark however the review ends — finished,
/// failed, cancelled (its task aborted) or panicked.
struct Claim {
    state: Arc<AppState>,
    idx: usize,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(b) = self.state.backends.lock_or_recover().get_mut(self.idx) {
            b.review_running = false;
        }
        self.state.review.wake.notify_one();
    }
}

fn launch(state: &Arc<AppState>, id: String, idx: usize, label: String) {
    let review = &state.review;
    let Some(job) = review.update(&id, |j| {
        j.status = Status::Running;
        j.started_ms = Some(now_ms());
        j.backend = Some(label.clone());
        j.wait_reason = None;
        j.attempts += 1;
        j.error = None;
    }) else {
        if let Some(b) = state.backends.lock_or_recover().get_mut(idx) {
            b.review_running = false;
        }
        return;
    };
    info!("review: {} started on {label}", job.id);
    let claim = Claim { state: state.clone(), idx };
    let st = state.clone();
    let task_id = id.clone();
    // Registered before the task can finish, under the same lock it removes
    // itself with, so a fast failure cannot leave a stale entry behind.
    let mut aborts = review.aborts.lock_or_recover();
    let handle = tokio::spawn(async move {
        let _claim = claim;
        let outcome = run(&st, &job, idx).await;
        let review = &st.review;
        review.aborts.lock_or_recover().remove(&task_id);
        let finished = review.update(&task_id, |j| {
            if j.status != Status::Running {
                return; // cancelled meanwhile
            }
            j.finished_ms = Some(now_ms());
            match outcome {
                Ok(out) => {
                    j.status = Status::Done;
                    j.result = Some(out.text);
                    j.prompt_tokens = out.prompt_tokens;
                    j.completion_tokens = out.completion_tokens;
                }
                Err(e) => {
                    j.status = Status::Failed;
                    j.error = Some(e);
                }
            }
        });
        if let Some(j) = finished {
            match &j.error {
                Some(e) => warn!("review: {} failed: {e}", j.id),
                None => info!("review: {} done", j.id),
            }
        }
    });
    aborts.insert(id, handle.abort_handle());
}

struct Outcome {
    text: String,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

async fn run(state: &Arc<AppState>, job: &Job, idx: usize) -> Result<Outcome, String> {
    let cfg = state.review.settings();
    let st = state.clone();
    let key = job.conv_key.clone();
    let conv = tokio::task::spawn_blocking(move || st.convlog.load_session(&key))
        .await
        .map_err(|e| format!("reading the conversation failed: {e}"))?
        .ok_or("the conversation is no longer in the log")?;
    let t = transcript::build(&conv, job.rid_to, cfg.max_transcript_chars, cfg.max_message_chars);
    if t.text.trim().is_empty() {
        return Err("the conversation has no recorded text to review".into());
    }
    state.review.update(&job.id, |j| {
        j.input_chars = Some(t.text.chars().count());
        j.truncated = t.truncated;
    });
    let (url, token, ollama, model) = {
        let backends = state.backends.lock_or_recover();
        let b = backends.get(idx).ok_or("the backend is gone")?;
        let ollama = b.api_type.is_empty() || b.api_type.contains(ApiFamily::Ollama);
        let model = crate::control::resolve_model_name(b, &job.model).unwrap_or_else(|| job.model.clone());
        (b.url.clone(), b.token.clone(), ollama, model)
    };
    let messages = json!([
        { "role": "system", "content": job.prompt_text },
        { "role": "user", "content": format!(
            "Conversation `{}` ({} turns{}):\n\n{}",
            job.conv_key,
            t.turns,
            if t.truncated { ", shortened to fit" } else { "" },
            t.text
        ) },
    ]);
    let (path, body) = if ollama {
        ("/api/chat", json!({ "model": model, "messages": messages, "stream": false }))
    } else {
        ("/v1/chat/completions", json!({ "model": model, "messages": messages, "stream": false }))
    };
    let mut headers = axum::http::HeaderMap::new();
    crate::dispatcher::attach_backend_token(&mut headers, token.as_deref());
    let res = state
        .client
        .post(format!("{}{path}", url.trim_end_matches('/')))
        .headers(headers)
        .timeout(std::time::Duration::from_secs(cfg.timeout_secs))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("the backend did not answer: {e}"))?;
    let status = res.status();
    let text = res.text().await.map_err(|e| format!("the reply could not be read: {e}"))?;
    if !status.is_success() {
        let mut why: String = text.chars().take(300).collect();
        if why.is_empty() {
            why = status.to_string();
        }
        return Err(format!("the backend answered {status}: {why}"));
    }
    parse_answer(&text)
}

/// The review text and token counts from an Ollama or OpenAI chat answer. A
/// reasoning model's `<think>` block is not part of the review.
fn parse_answer(text: &str) -> Result<Outcome, String> {
    let v: Value = serde_json::from_str(text).map_err(|_| "the backend's reply is not JSON".to_string())?;
    let content = v
        .pointer("/message/content")
        .or_else(|| v.pointer("/choices/0/message/content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let content = match (content.find("<think>"), content.find("</think>")) {
        (Some(a), Some(b)) if a < b => format!("{}{}", &content[..a], &content[b + "</think>".len()..]),
        _ => content.to_string(),
    };
    let content = content.trim().to_string();
    if content.is_empty() {
        return Err("the model answered nothing".into());
    }
    let n = |p: &str| v.pointer(p).and_then(Value::as_u64);
    Ok(Outcome {
        text: content,
        prompt_tokens: n("/prompt_eval_count").or_else(|| n("/usage/prompt_tokens")),
        completion_tokens: n("/eval_count").or_else(|| n("/usage/completion_tokens")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> BackendStatus {
        let mut b = BackendStatus::new("http://gpu:11434".into(), None, Vec::new());
        b.api_type = crate::dispatcher::ApiSet::OLLAMA;
        b.available_models = Arc::new(["qwen3:32b".to_string(), "llama3:8b".to_string()].into_iter().collect());
        b.loaded_state_known = true;
        b.last_user_ms = 0;
        b
    }

    const NOW: u64 = 10_000_000;

    #[test]
    fn a_review_waits_for_a_quiet_idle_backend() {
        let cfg = ReviewSettings::default();
        let mut b = backend();
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", false, false, false), Ok(()));
        assert_eq!(eligible(&b, NOW, &cfg, "missing", false, false, false), Err(Wait::NoModel));
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", true, false, false), Err(Wait::ControlOp));
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", false, true, false), Err(Wait::UserQueued));
        b.active_requests = 1;
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", false, false, false), Err(Wait::Busy));
        b.active_requests = 0;
        b.review_running = true;
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", false, false, false), Err(Wait::ReviewRunning));
        b.review_running = false;
        b.last_user_ms = NOW - 60_000;
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", false, false, false), Err(Wait::Quiet(240)));
        b.is_online = false;
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", false, false, false), Err(Wait::Offline));
    }

    #[test]
    fn a_review_evicts_a_users_model_only_after_the_long_idle() {
        let cfg = ReviewSettings::default();
        let mut b = backend();
        b.loaded_models = Arc::new(["llama3:8b".to_string()].into_iter().collect());
        b.last_user_ms = NOW - 400_000;
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", false, false, false), Err(Wait::Swap(200)));
        // Its own model loaded: no swap, so the short quiet is enough.
        assert_eq!(eligible(&b, NOW, &cfg, "llama3:8b", false, false, false), Ok(()));
        b.last_user_ms = NOW - 700_000;
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", false, false, false), Ok(()));
        // Nothing loaded: nothing to evict.
        b.last_user_ms = NOW - 400_000;
        b.loaded_models = Arc::default();
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", false, false, false), Ok(()));
    }

    #[test]
    fn run_now_skips_every_wait_but_an_unusable_backend() {
        let cfg = ReviewSettings::default();
        let mut b = backend();
        b.active_requests = 2;
        b.review_running = true;
        b.last_user_ms = NOW - 1_000;
        b.loaded_models = Arc::new(["llama3:8b".to_string()].into_iter().collect());
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", false, true, true), Ok(()));
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", false, true, false), Err(Wait::ReviewRunning));
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", true, false, true), Err(Wait::ControlOp));
        assert_eq!(eligible(&b, NOW, &cfg, "missing", false, false, true), Err(Wait::NoModel));
        b.is_online = false;
        assert_eq!(eligible(&b, NOW, &cfg, "qwen3:32b", false, false, true), Err(Wait::Offline));
    }

    #[test]
    fn the_nearest_reason_is_the_one_shown() {
        assert!(Wait::Quiet(100) < Wait::Swap(5));
        assert!(Wait::Busy < Wait::NoModel);
        assert_eq!(Wait::Quiet(133).describe("m"), "waiting for 2:13 more of quiet");
    }

    #[test]
    fn answers_are_read_from_both_apis() {
        let o = parse_answer(r#"{"message":{"content":"<think>hm</think>\n## Summary\nfine"},"prompt_eval_count":10,"eval_count":3}"#).unwrap();
        assert_eq!(o.text, "## Summary\nfine");
        assert_eq!((o.prompt_tokens, o.completion_tokens), (Some(10), Some(3)));
        let o = parse_answer(r#"{"choices":[{"message":{"content":"ok"}}],"usage":{"prompt_tokens":7,"completion_tokens":1}}"#).unwrap();
        assert_eq!((o.text.as_str(), o.prompt_tokens), ("ok", Some(7)));
        assert!(parse_answer(r#"{"message":{"content":"  "}}"#).is_err());
        assert!(parse_answer("nope").is_err());
    }
}
