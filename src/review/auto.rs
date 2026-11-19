//! Standing rules that turn finished conversations into reviews.
//!
//! A rule watches conversations that match its filters (backend, client, label)
//! and reviews each one — or a batch of them in one call — after the
//! conversation has gone quiet for its cooldown. The conversation is considered
//! over at `cooldown` after its last turn: agent sessions pause for minutes at
//! a time, and reviewing a live session reviews it half.
//!
//! Each rule reviews each conversation once. A review that failed or was
//! cancelled does not count as covered — the operator can Re-run it — but the
//! sweep never re-submits it on its own, so a rule that cannot run (a deleted
//! model, say) fails once instead of retrying forever.
//!
//! The sweep runs inside the review worker's loop, at most once a minute, and
//! only looks at conversations that ended within the last day: auto-reviews
//! are a watching habit, not a backfill tool — the manual review still covers
//! everything older.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use tracing::{info, warn};

use crate::clients::now_ms;
use crate::convlog::ConvSummary;
use crate::dispatcher::AppState;
use crate::lock::LockExt;
use crate::model_policy::glob_match;

use super::store::AutoRule;
use super::{AutoSpec, Due, Job, Status};

/// How often the sweep may run.
pub const SWEEP_SECS: u64 = 60;
/// A batch closes this long after its first conversation fell due, even if it
/// never filled up — a digest should not wait forever for company.
pub const BATCH_WINDOW_SECS: u64 = 300;
/// Conversations whose last turn is older than this are left to manual review.
pub const LOOKBACK_HOURS: u64 = 24;

/// The verdict an auto-review is asked to end with. Appended to the rule's
/// prompt at run time (never stored into it), and read back by
/// [`parse_attention`].
pub const ATTENTION_INSTRUCTION: &str = "

Finish your review with exactly one final line, one of:
ATTENTION: YES — <one line on what needs the operator's attention>
ATTENTION: NO — <one line on what the conversation(s) were about>
For a batch of conversations, one line for the whole batch, naming which conversations need attention.";

fn matches_ci(pattern: &str, text: &str) -> bool {
    glob_match(pattern, text)
}

/// Do the conversation's attributes satisfy the rule's filters?
pub fn matches(rule: &AutoRule, s: &ConvSummary) -> bool {
    if !rule.backend.is_empty()
        && !s.backend.as_deref().is_some_and(|b| b.eq_ignore_ascii_case(&rule.backend))
    {
        return false;
    }
    if !rule.client.is_empty() && s.user != rule.client {
        return false;
    }
    if !rule.label.is_empty()
        && !s.session_label.as_deref().is_some_and(|l| matches_ci(&rule.label, l))
    {
        return false;
    }
    true
}

/// Is the conversation finished and old enough to review — but not so old it
/// belongs to the manual page of history?
pub fn is_finished(rule: &AutoRule, s: &ConvSummary, now: u64) -> bool {
    if s.live || s.redacted || s.turns == 0 {
        return false;
    }
    let quiet = now.saturating_sub(s.end_ms);
    quiet >= rule.cooldown_mins * 60_000 && quiet <= LOOKBACK_HOURS * 3_600_000
}

/// The conversations each rule already has a claim on: every auto review that
/// is not failed or cancelled covers its conversations for its rule, for good.
pub fn covered(jobs: &BTreeMap<String, Job>) -> HashMap<String, HashSet<String>> {
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    for j in jobs.values() {
        if !j.auto {
            continue;
        }
        if matches!(j.status, Status::Failed | Status::Cancelled) {
            continue;
        }
        let Some(rule) = &j.rule_id else { continue };
        let keys = out.entry(rule.clone()).or_default();
        keys.insert(j.conv_key.clone());
        for p in &j.parts {
            keys.insert(p.key.clone());
        }
    }
    out
}

/// Take the conversations a batch job should go out with: filled batches at
/// once, everything that has waited past the window as one late batch.
pub fn drain(pending: &mut Vec<Due>, batch: u32, now: u64) -> Vec<Vec<Due>> {
    let mut out = Vec::new();
    if batch <= 1 {
        out.extend(pending.drain(..).map(|d| vec![d]));
        return out;
    }
    while pending.len() >= batch as usize {
        out.push(pending.drain(..batch as usize).collect());
    }
    if !pending.is_empty() && now.saturating_sub(pending[0].due_ms) >= BATCH_WINDOW_SECS * 1000 {
        out.push(std::mem::take(pending));
    }
    out
}

/// The verdict line of a review answer, if it has one: the last `ATTENTION:
/// YES|NO` anywhere in the text (the instruction itself quotes the words, so
/// only the final one can be the verdict). `None` when no verdict can be read
/// — the review then shows no badge rather than a guessed one.
pub fn parse_attention(text: &str) -> Option<(bool, String)> {
    const NEEDLE: &[u8] = b"ATTENTION:";
    let b = text.as_bytes();
    let mut at = None;
    let mut i = 0;
    while i + NEEDLE.len() <= b.len() {
        if b[i..i + NEEDLE.len()].eq_ignore_ascii_case(NEEDLE) {
            at = Some(i);
        }
        i += 1;
    }
    let at = at?;
    let rest = text[at + NEEDLE.len()..].trim_start();
    let is_cut = |c: Option<char>| c.is_none_or(|c| matches!(c, ' ' | '\t' | ':' | '-' | '–' | '—' | '.'));
    let yes = rest.get(..3).is_some_and(|r| r.eq_ignore_ascii_case("YES")) && is_cut(rest.chars().nth(3));
    let no = !yes && rest.get(..2).is_some_and(|r| r.eq_ignore_ascii_case("NO")) && is_cut(rest.chars().nth(2));
    if !yes && !no {
        return None;
    }
    let note: String = rest.chars().skip(if yes { 3 } else { 2 }).collect();
    let note = note
        .trim_start_matches([' ', '\t', ':', '-', '–', '—'])
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let note: String = note.chars().take(280).collect();
    Some((yes, note))
}

/// Look over the conversations for the standing rules and send what is due.
/// Called from the review worker's loop; does nothing before the next minute.
pub fn sweep(state: &Arc<AppState>) {
    let now = now_ms();
    let review = &state.review;
    if now.saturating_sub(review.auto_clock.load(Relaxed)) < SWEEP_SECS * 1000 {
        return;
    }
    review.auto_clock.store(now, Relaxed);
    let rules: Vec<AutoRule> = review.rules().into_iter().filter(|r| r.enabled).collect();
    if rules.is_empty() {
        return;
    }
    let cfg_model = review.settings().model;
    let summaries = state.convlog.summaries(usize::MAX);
    let claims = covered(&review.jobs.lock_or_recover());
    let mut sent = false;
    for rule in rules {
        // The spec is resolved every sweep so a renamed prompt or model edit
        // takes effect on the next conversation. A rule that cannot build one
        // (model gone, prompt deleted) says so once, and its conversations
        // wait — a later fix picks them up inside the lookback window.
        let spec = spec_for(review, &rule, &cfg_model);
        let spec = match spec {
            Ok(s) => s,
            Err(e) => {
                let mut warned = review.auto_warned.lock_or_recover();
                if warned.insert(format!("{}\u{0}", rule.id)) {
                    warn!("auto-review: rule '{}': {e}", rule.name);
                }
                continue;
            }
        };
        let already = claims.get(&rule.id);
        let mut pending = review.auto_pending.lock_or_recover().remove(&rule.id).unwrap_or_default();
        pending.retain(|d| now.saturating_sub(d.due_ms) < LOOKBACK_HOURS * 3_600_000);
        let have: HashSet<String> = pending.iter().map(|d| d.key.clone()).collect();
        for s in &summaries {
            if !is_finished(&rule, s, now) || !matches(&rule, s) {
                continue;
            }
            if have.contains(s.key.as_str()) || already.is_some_and(|c| c.contains(&s.key)) {
                continue;
            }
            pending.push(Due { key: s.key.clone(), due_ms: now });
        }
        let mut backs = Vec::new();
        for group in drain(&mut pending, rule.batch, now) {
            // The send re-reads the log: turn bounds are as they are at send
            // time, and a conversation deleted or redacted meanwhile drops out.
            let keys: Vec<String> = group.iter().map(|d| d.key.clone()).collect();
            match review.send_auto(state, &spec, &keys) {
                Ok(job) => {
                    sent = true;
                    let mut warned = review.auto_warned.lock_or_recover();
                    for d in &group {
                        warned.remove(&format!("{}\u{0}{}", rule.id, d.key));
                    }
                    info!(
                        "auto-review: {} queued for {} conversation(s) on {}",
                        job.id,
                        keys.len(),
                        job.model
                    );
                }
                Err(e) => {
                    let mut warned = review.auto_warned.lock_or_recover();
                    for d in &group {
                        if warned.insert(format!("{}\u{0}{}", rule.id, d.key)) {
                            warn!("auto-review: rule '{}', {}: {e}", rule.name, d.key);
                        }
                        backs.push(d.clone());
                    }
                }
            }
        }
        if !pending.is_empty() || !backs.is_empty() {
            pending.extend(backs);
            review.auto_pending.lock_or_recover().insert(rule.id.clone(), pending);
        }
    }
    if sent {
        review.wake.notify_one();
    }
}

/// Resolve a rule's prompt and model as they are right now.
pub(super) fn spec_for(review: &super::Review, rule: &AutoRule, cfg_model: &str) -> Result<AutoSpec, String> {
    let model = if rule.model.is_empty() { cfg_model } else { &rule.model }.trim().to_string();
    if model.is_empty() {
        return Err("no review model: set one on the rule or in Settings".into());
    }
    let prompts = review.prompts.lock_or_recover();
    let chosen = if rule.prompt_id.is_empty() {
        prompts.default_prompt()
    } else {
        prompts.get(&rule.prompt_id).ok_or_else(|| format!("no prompt '{}'", rule.prompt_id))?
    };
    Ok(AutoSpec {
        rule_id: Some(rule.id.clone()),
        rule_name: Some(rule.name.clone()),
        prompt_id: Some(chosen.id.clone()),
        prompt_name: chosen.name.clone(),
        prompt_text: chosen.text.clone(),
        model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convlog::ConvSummary;
    use crate::review::ConvRef;

    const MIN: u64 = 60_000;

    fn rule(f: impl FnOnce(&mut AutoRule)) -> AutoRule {
        let mut r = AutoRule {
            id: "r".into(),
            name: "rule".into(),
            enabled: true,
            backend: String::new(),
            client: String::new(),
            label: String::new(),
            prompt_id: "default".into(),
            model: "m".into(),
            cooldown_mins: 30,
            batch: 1,
        };
        f(&mut r);
        r
    }

    fn conv(f: impl FnOnce(&mut ConvSummary)) -> ConvSummary {
        ConvSummary {
            key: "alice#a1".into(),
            user: "alice".into(),
            session: Some("a1".into()),
            session_label: None,
            model: Some("m".into()),
            backend: Some("gpu".into()),
            priority: None,
            turns: 3,
            start_ms: 0,
            end_ms: 0,
            redacted: false,
            live: false,
            dropped_turns: 0,
            flags: vec![],
        }
        .tap(f)
    }

    trait Tap: Sized {
        fn tap(mut self, f: impl FnOnce(&mut Self)) -> Self {
            f(&mut self);
            self
        }
    }
    impl<T> Tap for T {}

    #[test]
    fn filters_are_what_the_rule_watches() {
        let c = conv(|_| {});
        assert!(matches(&rule(|_| {}), &c), "an empty rule matches everything");
        assert!(matches(&rule(|r| r.backend = "GPU".into()), &c), "backend is case-insensitive");
        assert!(!matches(&rule(|r| r.backend = "backup".into()), &c));
        assert!(matches(&rule(|r| r.client = "alice".into()), &c));
        assert!(!matches(&rule(|r| r.client = "bob".into()), &c));
        assert!(matches(&rule(|r| r.label = "ci-*".into()), &conv(|c| c.session_label = Some("ci-build".into()))));
        assert!(
            !matches(&rule(|r| r.label = "ci-*".into()), &c),
            "a conversation without a label never matches a set pattern"
        );
    }

    #[test]
    fn a_conversation_is_due_after_its_cooldown_and_not_past_the_lookback() {
        let r = rule(|_| {});
        // Far enough from zero that the lookback subtractions stay in range.
        let now = 100 * LOOKBACK_HOURS * 60 * MIN;
        assert!(!is_finished(&r, &conv(|c| c.end_ms = now - 29 * MIN), now), "still cooling down");
        assert!(is_finished(&r, &conv(|c| c.end_ms = now - 30 * MIN), now));
        assert!(is_finished(&r, &conv(|c| c.end_ms = now - LOOKBACK_HOURS * 60 * MIN), now), "exactly at the lookback edge is still watched");
        assert!(!is_finished(&r, &conv(|c| c.end_ms = now - (LOOKBACK_HOURS + 1) * 60 * MIN), now), "older than the lookback goes to the manual page");
        assert!(!is_finished(&r, &conv(|c| c.live = true), now));
        assert!(!is_finished(&r, &conv(|c| c.redacted = true), now));
        assert!(!is_finished(&r, &conv(|c| c.turns = 0), now));
        let short = rule(|r| r.cooldown_mins = 5);
        assert!(is_finished(&short, &conv(|c| c.end_ms = now - 6 * MIN), now));
    }

    #[test]
    fn a_rule_keeps_its_claim_except_over_failures() {
        let mut jobs = BTreeMap::new();
        let mut job = |id: &str, status: Status, parts: Vec<&str>| {
            let j = Job {
                id: id.into(),
                conv_key: parts[0].into(),
                user: "u".into(),
                label: None,
                rid_from: 1,
                rid_to: 1,
                turns: 1,
                prompt_id: None,
                prompt_name: "p".into(),
                prompt_text: "t".into(),
                custom: false,
                model: "m".into(),
                status,
                wait_reason: None,
                backend: None,
                created_ms: 0,
                started_ms: None,
                finished_ms: None,
                input_chars: None,
                truncated: false,
                result: None,
                error: None,
                prompt_tokens: None,
                completion_tokens: None,
                attempts: 1,
                run_now: false,
                auto: true,
                rule_id: Some("r".into()),
                rule_name: Some("rule".into()),
                attention: None,
                attention_note: None,
                parts: parts[1..].iter().map(|k| ConvRef { key: (*k).into(), rid_to: 2 }).collect(),
            };
            jobs.insert(id.to_string(), j);
        };
        job("done", Status::Done, vec!["alice#a1"]);
        job("failed", Status::Failed, vec!["alice#a2"]);
        job("batch", Status::Queued, vec!["alice#a3", "alice#a4"]);
        let covered = covered(&jobs);
        let mine = covered.get("r").unwrap();
        assert!(mine.contains("alice#a1") && mine.contains("alice#a3") && mine.contains("alice#a4"));
        assert!(!mine.contains("alice#a2"), "a failed review leaves the door open for a re-run");
    }

    fn due(key: &str, due_ms: u64) -> Due {
        Due { key: key.into(), due_ms }
    }

    #[test]
    fn batches_close_when_full_or_when_the_window_passes() {
        let mut p = vec![due("a", 0), due("b", 10), due("c", 20)];
        let out = drain(&mut p, 2, 10_000);
        assert_eq!(out.len(), 1, "one filled batch");
        assert_eq!(out[0].len(), 2);
        assert_eq!(p.len(), 1, "the third waits for company");
        let late = drain(&mut p, 2, 20 + BATCH_WINDOW_SECS * 1000);
        assert_eq!(late[0].len(), 1, "the window sends the leftover alone");
        assert!(p.is_empty());

        let mut p = vec![due("a", 0), due("b", 0), due("c", 0)];
        let out = drain(&mut p, 1, 0);
        assert_eq!(out.len(), 3, "batch 1 sends each conversation as its own review");
        assert!(p.is_empty());
    }

    #[test]
    fn the_verdict_line_is_read_from_the_answer() {
        let yes = "## Problems\n…\n\nATTENTION: YES — it deleted the test to pass the build";
        let (v, note) = parse_attention(yes).unwrap();
        assert!(v);
        assert_eq!(note, "it deleted the test to pass the build");
        let (v, note) = parse_attention("ATTENTION:no").unwrap();
        assert!(!v);
        assert_eq!(note, "");
        assert!(parse_attention("ATTENTION: NOTHING to report").is_none(), "a word is not a verdict");
        assert!(parse_attention("no verdict here").is_none());
        // The instruction itself quotes the words; the last mention decides.
        let quoted = "End with:\nATTENTION: YES — example\nATTENTION: no — nothing actually wrong";
        let (v, _) = parse_attention(quoted).unwrap();
        assert!(!v, "the quoted example does not count");
    }
}
