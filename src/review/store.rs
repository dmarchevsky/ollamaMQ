//! What reviews keep on disk: one JSON file per review job, and the prompt
//! library as a YAML store next to `appconf.yaml`.
//!
//! Jobs live in `<logs>/reviews/`, outside `sessions/`, so retention and a
//! conversation's Delete never take a review with them: a review outlives what
//! it reviewed, and says so when asked to run again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// One review: the slice of a conversation it covers, the prompt and model it
/// was sent with, and what came back.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub conv_key: String,
    pub user: String,
    /// The conversation's own label (`X-Session-Name`), for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// First and last turn reviewed. The last is fixed when the review is sent,
    /// so a conversation that keeps going gets the review it was sent for.
    pub rid_from: u64,
    pub rid_to: u64,
    pub turns: usize,
    /// The library prompt it started from; `None` with `custom` for a prompt
    /// written for this review alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    pub prompt_name: String,
    /// The text actually sent — a copy, so editing the library later does not
    /// rewrite what an old review was asked.
    pub prompt_text: String,
    #[serde(default)]
    pub custom: bool,
    pub model: String,
    pub status: Status,
    /// Why a queued review is not running yet. Live only: refreshed by the
    /// worker each tick and never read back from disk.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u64>,
    /// Characters of transcript handed to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_chars: Option<usize>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub attempts: u32,
    /// The operator pressed Run now: the review starts on the first backend
    /// that has its model, without waiting for quiet (see [`crate::review::eligible`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub run_now: bool,
    /// Sent by a standing rule rather than by hand.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// Kept for display: the rule may be gone while its reviews are not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_name: Option<String>,
    /// What the finished review said about needing attention: parsed from the
    /// verdict line an auto-review asks for. `None` when there was no verdict
    /// line to read — no badge rather than a guessed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention_note: Option<String>,
    /// The conversations this review covers with their turn bounds. Empty on
    /// manual reviews, which cover `conv_key` alone as before; an automatic
    /// one always lists its parts, a batch as several.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ConvRef>,
}

/// One conversation inside a review, and the turn it was reviewed up to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConvRef {
    pub key: String,
    pub rid_to: u64,
}

impl Job {
    /// The job as the list shows it: everything but the long texts.
    pub fn summary(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.remove("result");
            o.remove("prompt_text");
            o.insert("has_result".into(), self.result.is_some().into());
        }
        v
    }
}

/// An id is a file name: only these characters are ever accepted back.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Write one job as `<dir>/<id>.json`: a temporary file renamed over the old.
pub fn save_job(dir: &Path, job: &Job) {
    if !valid_id(&job.id) {
        return;
    }
    let path = dir.join(format!("{}.json", job.id));
    let tmp = dir.join(format!("{}.json.tmp", job.id));
    let ok = serde_json::to_vec_pretty(job)
        .ok()
        .is_some_and(|b| std::fs::write(&tmp, b).is_ok() && std::fs::rename(&tmp, &path).is_ok());
    if !ok {
        warn!("review: could not save {}", path.display());
    }
}

pub fn delete_job(dir: &Path, id: &str) {
    if valid_id(id) {
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
    }
}

/// Every job in `dir`. One that was running when the proxy stopped is queued
/// again: its call died with the process.
pub fn load_jobs(dir: &Path) -> Vec<Job> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read(&p).map_err(|e| e.to_string()).and_then(|b| {
            serde_json::from_slice::<Job>(&b).map_err(|e| e.to_string())
        }) {
            Ok(mut job) => {
                if job.status == Status::Running {
                    job.status = Status::Queued;
                    job.started_ms = None;
                    job.backend = None;
                }
                out.push(job);
            }
            Err(e) => warn!("review: skipping {}: {e}", p.display()),
        }
    }
    out
}

/// One reusable review prompt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    pub id: String,
    pub name: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PromptFile {
    /// Id of the prompt the Review dialog starts from.
    #[serde(default)]
    pub default: String,
    #[serde(default)]
    pub prompts: Vec<Prompt>,
}

pub const DEFAULT_PROMPTS_FILE: &str = "review_prompts.yaml";

/// The prompt every library starts with.
pub const STARTER_PROMPT: &str = "You are reviewing a recorded conversation between a user \
(often a coding agent's harness) and an AI assistant, captured by an LLM proxy. The transcript \
follows. Each turn shows only the messages it added; long messages and tool results are cut \
where you see a [… cut …] marker.

Write a concise review in Markdown with these sections:

## Summary
What the user wanted, and whether it was achieved.

## What went well

## Problems
Mistakes, wrong or unsupported claims, wasted steps, loops, tool misuse, ignored instructions. \
Name the turn for each.

## Suggestions
Concrete changes to prompts, tools or settings that would have helped.

## Verdict
One line: success, partial or failure, with a one-sentence reason.";

/// The prompt library: in memory, written back on every change when it has a
/// file.
pub struct Prompts {
    path: Option<PathBuf>,
    pub file: PromptFile,
}

impl Prompts {
    pub fn in_memory() -> Self {
        let mut p = Self { path: None, file: PromptFile::default() };
        p.seed();
        p
    }

    /// Read the library from `path`, starting one when there is none. An
    /// unreadable file is kept as it is and the library runs in memory, so a
    /// typo never costs the operator their prompts.
    pub fn load(path: PathBuf) -> Self {
        match crate::clients::read_store_or_migrate::<PromptFile>(&path) {
            Ok(Some(file)) => {
                let mut p = Self { path: Some(path), file };
                if p.seed() {
                    p.save();
                }
                p
            }
            Ok(None) => {
                let mut p = Self { path: Some(path), file: PromptFile::default() };
                p.seed();
                p.save();
                p
            }
            Err(e) => {
                warn!("review: {e}; the prompt library runs in memory until it is fixed");
                Self::in_memory()
            }
        }
    }

    /// Make sure there is a prompt and a valid default. True when it changed.
    fn seed(&mut self) -> bool {
        let mut changed = false;
        if self.file.prompts.is_empty() {
            self.file.prompts.push(Prompt {
                id: "default".into(),
                name: "Default review".into(),
                text: STARTER_PROMPT.into(),
            });
            changed = true;
        }
        if !self.file.prompts.iter().any(|p| p.id == self.file.default) {
            self.file.default = self.file.prompts[0].id.clone();
            changed = true;
        }
        changed
    }

    fn save(&self) {
        if let Some(path) = &self.path
            && let Err(e) = crate::clients::write_store_atomic(path, &self.file)
        {
            warn!("review: could not save {}: {e}", path.display());
        }
    }

    pub fn get(&self, id: &str) -> Option<&Prompt> {
        self.file.prompts.iter().find(|p| p.id == id)
    }

    pub fn default_prompt(&self) -> &Prompt {
        self.get(&self.file.default).unwrap_or(&self.file.prompts[0])
    }

    /// Add a prompt (no `id`) or change one. Returns its id.
    pub fn upsert(&mut self, id: Option<&str>, name: &str, text: &str, make_default: bool) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 80 {
            return Err("a prompt needs a name of at most 80 characters".into());
        }
        if text.trim().is_empty() {
            return Err("a prompt needs some text".into());
        }
        let id = match id {
            Some(id) => {
                let p = self
                    .file
                    .prompts
                    .iter_mut()
                    .find(|p| p.id == id)
                    .ok_or_else(|| format!("no prompt '{id}'"))?;
                p.name = name.to_string();
                p.text = text.to_string();
                id.to_string()
            }
            None => {
                let id = self.fresh_id(name);
                self.file.prompts.push(Prompt { id: id.clone(), name: name.to_string(), text: text.to_string() });
                id
            }
        };
        if make_default {
            self.file.default = id.clone();
        }
        self.save();
        Ok(id)
    }

    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        if self.file.prompts.len() <= 1 {
            return Err("the last prompt cannot be removed".into());
        }
        let before = self.file.prompts.len();
        self.file.prompts.retain(|p| p.id != id);
        if self.file.prompts.len() == before {
            return Err(format!("no prompt '{id}'"));
        }
        self.seed();
        self.save();
        Ok(())
    }

    /// A slug of the name, unique in the library.
    fn fresh_id(&self, name: &str) -> String {
        let taken: BTreeMap<&str, ()> = self.file.prompts.iter().map(|p| (p.id.as_str(), ())).collect();
        fresh_id(name, "prompt", |id| taken.contains_key(id))
    }
}

/// A slug of the name used as a file-safe id, `fallback` when nothing remains
/// of it, and `n` appended until unique. `taken` says which ids are.
fn fresh_id(name: &str, fallback: &str, taken: impl Fn(&str) -> bool) -> String {
    let mut base: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    base.truncate(40);
    if base.is_empty() {
        base = fallback.into();
    }
    if !taken(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|c| !taken(c)).unwrap_or(base)
}

// ---------------------------------------------------------- auto-review rules

/// Where the standing rules live, next to the prompt library.
pub const DEFAULT_RULES_FILE: &str = "review_rules.yaml";

/// Default quiet a conversation must have seen before a rule reviews it: past
/// the pauses an agent session makes, so a rule reviews a live session whole,
/// once.
pub const DEFAULT_COOLDOWN_MINS: u64 = 30;

fn cooldown_default() -> u64 {
    DEFAULT_COOLDOWN_MINS
}

/// A standing rule: which conversations to review, with what, once they have
/// gone quiet. An empty filter field means "any".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutoRule {
    /// Slug of the name, assigned on create.
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default = "yes_bool")]
    pub enabled: bool,
    /// The backend that served the conversation (its label), case-insensitive.
    #[serde(default)]
    pub backend: String,
    /// The client that owns the conversation, exact name.
    #[serde(default)]
    pub client: String,
    /// Glob (`*`, `?`) over the conversation label; an unlabeled conversation
    /// never matches a set pattern.
    #[serde(default)]
    pub label: String,
    /// Library prompt to start from; empty means the library's default.
    #[serde(default)]
    pub prompt_id: String,
    /// Review model; empty means `review.model` from Settings.
    #[serde(default)]
    pub model: String,
    /// Quiet after the conversation's last turn before it is due.
    #[serde(default = "cooldown_default")]
    pub cooldown_mins: u64,
    /// Due conversations reviewed together in one call; 1 is one review each.
    #[serde(default)]
    pub batch: u32,
}

fn yes_bool() -> bool {
    true
}

impl AutoRule {
    /// Clean the fields into their stored shape and reject what cannot work.
    pub fn validate(&mut self) -> Result<(), String> {
        self.name = self.name.trim().to_string();
        self.backend = self.backend.trim().to_string();
        self.client = self.client.trim().to_string();
        self.label = self.label.trim().to_string();
        self.model = self.model.trim().to_string();
        if self.name.is_empty() || self.name.chars().count() > 80 {
            return Err("a rule needs a name of at most 80 characters".into());
        }
        if !(5..=1440).contains(&self.cooldown_mins) {
            return Err("the cooldown must be between 5 and 1440 minutes".into());
        }
        if !(1..=10).contains(&self.batch) {
            return Err("the batch size must be between 1 and 10".into());
        }
        Ok(())
    }

    /// A human line of what the rule watches, for the UI.
    pub fn watches(&self) -> String {
        let backend = if self.backend.is_empty() { "any backend" } else { &self.backend };
        let client = if self.client.is_empty() { "any client" } else { &self.client };
        let label = if self.label.is_empty() { "any label" } else { &self.label };
        format!("{backend} · {client} · {label}")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuleFile {
    #[serde(default)]
    pub rules: Vec<AutoRule>,
}

/// The standing rules: in memory, written back on every change when they have
/// a file. Like the prompt library, an unreadable file is left as it is and
/// the rules run in memory, so a typo never costs the operator their rules.
pub struct Rules {
    path: Option<PathBuf>,
    pub file: RuleFile,
}

impl Rules {
    pub fn in_memory() -> Self {
        Self { path: None, file: RuleFile::default() }
    }

    pub fn load(path: PathBuf) -> Self {
        match crate::clients::read_store_or_migrate::<RuleFile>(&path) {
            Ok(Some(file)) => Self { path: Some(path), file },
            Ok(None) => Self { path: Some(path), file: RuleFile::default() },
            Err(e) => {
                warn!("review: {e}; the auto-review rules run in memory until it is fixed");
                Self::in_memory()
            }
        }
    }

    fn save(&self) {
        if let Some(path) = &self.path
            && let Err(e) = crate::clients::write_store_atomic(path, &self.file)
        {
            warn!("review: could not save {}: {e}", path.display());
        }
    }

    pub fn get(&self, id: &str) -> Option<&AutoRule> {
        self.file.rules.iter().find(|r| r.id == id)
    }

    /// Add a rule (empty `id`) or change one. Returns its id.
    pub fn upsert(&mut self, mut rule: AutoRule) -> Result<String, String> {
        rule.validate()?;
        if rule.id.is_empty() {
            rule.id = fresh_id(&rule.name, "rule", |id| self.get(id).is_some());
        } else if self.get(&rule.id).is_none() {
            return Err(format!("no rule '{}'", rule.id));
        }
        let file = &mut self.file.rules;
        match file.iter_mut().find(|r| r.id == rule.id) {
            Some(slot) => *slot = rule.clone(),
            None => file.push(rule.clone()),
        }
        self.save();
        Ok(rule.id)
    }

    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        let before = self.file.rules.len();
        self.file.rules.retain(|r| r.id != id);
        if self.file.rules.len() == before {
            return Err(format!("no rule '{id}'"));
        }
        self.save();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, status: Status) -> Job {
        Job {
            id: id.into(),
            conv_key: "u#abc123".into(),
            user: "u".into(),
            label: None,
            rid_from: 1,
            rid_to: 5,
            turns: 5,
            prompt_id: Some("default".into()),
            prompt_name: "Default review".into(),
            prompt_text: "review it".into(),
            custom: false,
            model: "m".into(),
            status,
            wait_reason: Some("busy".into()),
            backend: Some("gpu".into()),
            created_ms: 1,
            started_ms: Some(2),
            finished_ms: None,
            input_chars: None,
            truncated: false,
            result: None,
            error: None,
            prompt_tokens: None,
            completion_tokens: None,
            attempts: 1,
            run_now: false,
            auto: false,
            rule_id: None,
            rule_name: None,
            attention: None,
            attention_note: None,
            parts: vec![],
        }
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("omq-review-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn jobs_round_trip_and_a_running_one_comes_back_queued() {
        let d = tmpdir("jobs");
        save_job(&d, &job("a-1", Status::Done));
        save_job(&d, &job("b-2", Status::Running));
        save_job(&d, &job("../evil", Status::Done));
        let mut jobs = load_jobs(&d);
        jobs.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].status, Status::Done);
        assert_eq!(jobs[1].status, Status::Queued);
        assert_eq!(jobs[1].backend, None);
        assert_eq!(jobs[0].wait_reason, None, "a wait reason is live only");
        delete_job(&d, "a-1");
        assert_eq!(load_jobs(&d).len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_library_starts_seeded_and_keeps_its_last_prompt() {
        let d = tmpdir("prompts");
        let path = d.join(DEFAULT_PROMPTS_FILE);
        let mut p = Prompts::load(path.clone());
        assert_eq!(p.default_prompt().id, "default");
        assert!(path.exists(), "the starter library is written for the operator to see");
        let id = p.upsert(None, "Security pass", "look for leaks", true).unwrap();
        assert_eq!(id, "security-pass");
        assert_eq!(p.upsert(None, "Security pass", "again", false).unwrap(), "security-pass-2");
        let p2 = Prompts::load(path);
        assert_eq!(p2.default_prompt().id, "security-pass");
        assert_eq!(p2.file.prompts.len(), 3);
        p.remove("default").unwrap();
        p.remove("security-pass").unwrap();
        assert_eq!(p.default_prompt().id, "security-pass-2", "a removed default falls to the first left");
        assert!(p.remove("security-pass-2").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn rule(name: &str) -> AutoRule {
        AutoRule {
            id: String::new(),
            name: name.into(),
            enabled: true,
            backend: String::new(),
            client: "alice".into(),
            label: String::new(),
            prompt_id: "default".into(),
            model: String::new(),
            cooldown_mins: DEFAULT_COOLDOWN_MINS,
            batch: 1,
        }
    }

    #[test]
    fn rules_round_trip_and_validate() {
        let d = tmpdir("rules");
        let path = d.join(DEFAULT_RULES_FILE);
        let mut r = Rules::load(path.clone());
        assert!(r.file.rules.is_empty(), "a fresh file has no rules");
        let id = r.upsert(rule("Agent sessions nightly")).unwrap();
        assert_eq!(id, "agent-sessions-nightly");
        assert_eq!(r.upsert(rule("Agent sessions nightly")).unwrap(), "agent-sessions-nightly-2");
        let mut off = rule("Agent sessions nightly");
        off.id = id.clone();
        off.enabled = false;
        off.batch = 4;
        assert_eq!(r.upsert(off).unwrap(), id);
        assert!(!r.get(&id).unwrap().enabled, "an update changes the rule in place");
        assert_eq!(Rules::load(path).get(&id).unwrap().batch, 4);

        let mut bad = rule("x");
        bad.name = String::new();
        assert!(r.upsert(bad).is_err());
        let mut bad = rule("x");
        bad.cooldown_mins = 1;
        assert!(r.upsert(bad).is_err());
        let mut bad = rule("x");
        bad.batch = 11;
        assert!(r.upsert(bad).is_err());
        assert!(r.upsert({
            let mut bad = rule("x");
            bad.id = "ghost".into();
            bad
        })
        .is_err());
        r.remove(&id).unwrap();
        assert!(r.remove(&id).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
