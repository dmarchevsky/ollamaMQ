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
            base = "prompt".into();
        }
        let taken: BTreeMap<&str, ()> = self.file.prompts.iter().map(|p| (p.id.as_str(), ())).collect();
        if !taken.contains_key(base.as_str()) {
            return base;
        }
        (2..).map(|n| format!("{base}-{n}")).find(|c| !taken.contains_key(c.as_str())).unwrap_or(base)
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
}
