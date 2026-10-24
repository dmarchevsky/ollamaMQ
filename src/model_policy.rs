//! Which models the proxy serves (`settings.models_file`, default
//! `models.yaml`), managed from the web UI's Models page.
//!
//! A rule is a case-insensitive glob (`*` = any run of characters, `?` = one
//! character) matched against the model names backends list, optionally
//! limited to some backends. **Hide** rules always win; **allow** rules only
//! matter in [`Mode::Allow`]:
//!
//! ```text
//! served(model, backend) = no hide rule hits
//!                          && (mode == Deny || some allow rule hits)
//! ```
//!
//! Keeping the two actions apart makes flipping the mode safe: the hide rules
//! keep hiding, and allow rules written for the other mode come back when it
//! is switched back. A hidden model is left out of the merged model listings,
//! and a request naming it is answered `404` at once (see the dispatcher).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::clients::{StoreError, now_ms, random_hex, read_store_or_migrate, write_store_atomic};

pub const DEFAULT_MODELS_FILE: &str = "models.yaml";

const MAX_PATTERN_LEN: usize = 200;

/// The policy's default for models no rule mentions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Serve every model except hidden ones.
    #[default]
    Deny,
    /// Serve only models an allow rule matches (and no hide rule does).
    Allow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Hide,
    Allow,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// Generated at load when a hand-written file leaves it out.
    #[serde(default)]
    pub id: String,
    pub pattern: String,
    pub action: Action,
    /// Backend URLs the rule is limited to; empty = every backend.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<String>,
    /// Created by a model's on/off switch (an exact name) rather than typed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_toggle: bool,
    #[serde(default, skip_serializing_if = "crate::clients::is_zero_ms")]
    pub created_at_ms: u64,
}

/// A rule in a hand-written file: just the pattern (hide, every backend), or
/// the full form.
#[derive(Deserialize)]
#[serde(untagged)]
enum RuleSpec {
    Pattern(String),
    Full(Rule),
}

fn de_rules<'de, D>(d: D) -> Result<Vec<Rule>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let specs = Vec::<RuleSpec>::deserialize(d)?;
    Ok(specs
        .into_iter()
        .map(|spec| match spec {
            RuleSpec::Pattern(pattern) => Rule {
                id: String::new(),
                pattern,
                action: Action::Hide,
                backends: Vec::new(),
                from_toggle: true,
                created_at_ms: 0,
            },
            RuleSpec::Full(r) => r,
        })
        .collect())
}

impl Rule {
    fn applies_to(&self, backend_url: &str) -> bool {
        self.backends.is_empty() || self.backends.iter().any(|b| b == backend_url)
    }

    fn hits(&self, model: &str, backend_url: &str) -> bool {
        self.applies_to(backend_url) && glob_match(&self.pattern, model)
    }

    fn is_exact_for(&self, model: &str) -> bool {
        self.pattern.eq_ignore_ascii_case(model)
    }
}

/// An immutable rule set, shared as an `Arc` so the request path reads it
/// without holding the store's lock.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default, deserialize_with = "de_rules")]
    pub rules: Vec<Rule>,
}

impl Policy {
    /// True when the policy serves everything: the fast path for the common
    /// case of no filtering at all.
    pub fn is_open(&self) -> bool {
        self.mode == Mode::Deny && !self.rules.iter().any(|r| r.action == Action::Hide)
    }

    /// A hide rule names this model on this backend.
    pub fn hidden(&self, model: &str, backend_url: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.action == Action::Hide && r.hits(model, backend_url))
    }

    /// Whether `model` (a name as the backend lists it) is served by the
    /// backend at `backend_url`.
    pub fn served(&self, model: &str, backend_url: &str) -> bool {
        if self.hidden(model, backend_url) {
            return false;
        }
        self.mode == Mode::Deny
            || self
                .rules
                .iter()
                .any(|r| r.action == Action::Allow && r.hits(model, backend_url))
    }
}

/// Case-insensitive glob match: `*` matches any run of characters (including
/// none), `?` exactly one; everything else is literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    // Last `*` seen and the text position it is currently stretched to.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((s, mark)) = star {
            // Let the last `*` swallow one more character and retry.
            pi = s + 1;
            ti = mark + 1;
            star = Some((s, mark + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

pub struct ModelPolicyStore {
    /// `None` keeps the store in memory only (tests).
    path: Option<PathBuf>,
    policy: Arc<Policy>,
}

impl ModelPolicyStore {
    pub fn in_memory() -> Self {
        Self {
            path: None,
            policy: Arc::default(),
        }
    }

    /// Load from `path`; a missing file is an open policy. A file that exists
    /// but cannot be parsed is an error rather than silently serving every
    /// model the operator meant to hide.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let mut policy = read_store_or_migrate::<Policy>(&path)?.unwrap_or_default();
        for r in &mut policy.rules {
            if r.id.is_empty() {
                r.id = format!("m{}", random_hex(6));
            }
        }
        Ok(Self {
            path: Some(path),
            policy: Arc::new(policy),
        })
    }

    pub fn snapshot(&self) -> Arc<Policy> {
        self.policy.clone()
    }

    pub fn set_mode(&mut self, mode: Mode) -> Result<(), StoreError> {
        let mut next = (*self.policy).clone();
        next.mode = mode;
        self.commit(next)
    }

    pub fn add_rule(
        &mut self,
        pattern: &str,
        action: Action,
        backends: Vec<String>,
    ) -> Result<Rule, StoreError> {
        let pattern = pattern.trim();
        if pattern.is_empty()
            || pattern.chars().count() > MAX_PATTERN_LEN
            || pattern.chars().any(char::is_control)
        {
            return Err(StoreError::Invalid(format!(
                "pattern must be 1–{MAX_PATTERN_LEN} characters"
            )));
        }
        let mut backends = backends;
        backends.sort();
        backends.dedup();
        let mut next = (*self.policy).clone();
        let duplicate = next.rules.iter().any(|r| {
            r.action == action && r.pattern.eq_ignore_ascii_case(pattern) && r.backends == backends
        });
        if duplicate {
            return Err(StoreError::Conflict("that rule already exists".into()));
        }
        let rule = Rule {
            id: format!("m{}", random_hex(6)),
            pattern: pattern.to_string(),
            action,
            backends,
            from_toggle: false,
            created_at_ms: now_ms(),
        };
        next.rules.push(rule.clone());
        self.commit(next)?;
        Ok(rule)
    }

    pub fn delete_rule(&mut self, id: &str) -> Result<(), StoreError> {
        let mut next = (*self.policy).clone();
        let before = next.rules.len();
        next.rules.retain(|r| r.id != id);
        if next.rules.len() == before {
            return Err(StoreError::NotFound(format!("no rule '{id}'")));
        }
        self.commit(next)
    }

    /// The per-model switch: serve or hide `model` on every backend by adding
    /// or removing exact-name rules.
    ///
    /// Switching a model on fails when a *pattern* hide rule for every
    /// backend still covers it — removing that rule is the operator's call,
    /// since it hides other models too.
    pub fn set_served(&mut self, model: &str, served: bool) -> Result<(), StoreError> {
        let model = model.trim();
        if model.is_empty() {
            return Err(StoreError::Invalid("model name is empty".into()));
        }
        let mut next = (*self.policy).clone();
        let toggle_rule = |action| Rule {
            id: format!("m{}", random_hex(6)),
            pattern: model.to_string(),
            action,
            backends: Vec::new(),
            from_toggle: true,
            created_at_ms: now_ms(),
        };
        if served {
            next.rules
                .retain(|r| !(r.action == Action::Hide && r.is_exact_for(model)));
            if let Some(r) = next.rules.iter().find(|r| {
                r.action == Action::Hide && r.backends.is_empty() && glob_match(&r.pattern, model)
            }) {
                return Err(StoreError::Conflict(format!(
                    "'{model}' is hidden by the rule '{}' — remove or narrow that rule",
                    r.pattern
                )));
            }
            let allowed = next.rules.iter().any(|r| {
                r.action == Action::Allow && r.backends.is_empty() && glob_match(&r.pattern, model)
            });
            if next.mode == Mode::Allow && !allowed {
                next.rules.push(toggle_rule(Action::Allow));
            }
        } else {
            // A leftover exact allow rule would only confuse: hide wins anyway.
            next.rules
                .retain(|r| !(r.action == Action::Allow && r.is_exact_for(model) && r.from_toggle));
            let already = next.rules.iter().any(|r| {
                r.action == Action::Hide && r.backends.is_empty() && r.is_exact_for(model)
            });
            if !already {
                next.rules.push(toggle_rule(Action::Hide));
            }
        }
        self.commit(next)
    }

    /// Persist `next`, then make it the live policy.
    fn commit(&mut self, next: Policy) -> Result<(), StoreError> {
        if next == *self.policy {
            return Ok(());
        }
        if let Some(path) = &self.path {
            write_store_atomic(path, &next)
                .map_err(|e| StoreError::Io(format!("cannot save {}: {e}", path.display())))?;
        }
        self.policy = Arc::new(next);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const A: &str = "http://a:1";
    const B: &str = "http://b:2";

    #[test]
    fn glob_matching() {
        assert!(glob_match("*", ""));
        assert!(glob_match("llama3*", "LLaMA3:latest"));
        assert!(glob_match("*-instruct-q6", "Qwen3.8-27B-Instruct-Q6"));
        assert!(glob_match("qwen?:32b", "qwen3:32b"));
        assert!(glob_match("*embed*", "nomic-embed-text:latest"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
        assert!(!glob_match("llama3", "llama3:latest"));
        assert!(!glob_match("?", ""));
    }

    #[test]
    fn hide_rules_win_and_scope_limits() {
        let mut s = ModelPolicyStore::in_memory();
        assert!(s.snapshot().is_open());
        s.add_rule("codellama*", Action::Hide, vec![]).unwrap();
        s.add_rule("qwen3.8-27b", Action::Hide, vec![B.into()])
            .unwrap();
        let p = s.snapshot();
        assert!(!p.is_open());
        assert!(!p.served("codellama:34b", A));
        assert!(p.served("Qwen3.8-27B", A));
        assert!(!p.served("Qwen3.8-27B", B));

        // Allow mode: only allowed models, and hide still wins.
        s.set_mode(Mode::Allow).unwrap();
        s.add_rule("*", Action::Allow, vec![A.into()]).unwrap();
        let p = s.snapshot();
        assert!(p.served("gemma3:27b", A));
        assert!(!p.served("gemma3:27b", B));
        assert!(!p.served("codellama:34b", A));
    }

    #[test]
    fn toggle_adds_and_removes_exact_rules() {
        let mut s = ModelPolicyStore::in_memory();
        s.set_served("llava:13b", false).unwrap();
        assert!(!s.snapshot().served("llava:13b", A));
        s.set_served("llava:13b", false).unwrap();
        assert_eq!(s.snapshot().rules.len(), 1, "no duplicate rule");
        s.set_served("LLaVA:13b", true).unwrap();
        assert!(s.snapshot().served("llava:13b", A));
        assert!(s.snapshot().rules.is_empty());

        // A pattern rule cannot be switched off from a single model.
        s.add_rule("llava*", Action::Hide, vec![]).unwrap();
        assert!(matches!(
            s.set_served("llava:13b", true),
            Err(StoreError::Conflict(_))
        ));

        // Allow mode: switching on adds an allow rule.
        let mut s = ModelPolicyStore::in_memory();
        s.set_mode(Mode::Allow).unwrap();
        assert!(!s.snapshot().served("phi4:14b", A));
        s.set_served("phi4:14b", true).unwrap();
        assert!(s.snapshot().served("phi4:14b", A));
    }

    #[test]
    fn validation_and_duplicates() {
        let mut s = ModelPolicyStore::in_memory();
        assert!(matches!(
            s.add_rule("  ", Action::Hide, vec![]),
            Err(StoreError::Invalid(_))
        ));
        s.add_rule("x*", Action::Hide, vec![A.into(), A.into()])
            .unwrap();
        assert_eq!(s.snapshot().rules[0].backends, vec![A.to_string()]);
        assert!(matches!(
            s.add_rule("X*", Action::Hide, vec![A.into()]),
            Err(StoreError::Conflict(_))
        ));
        let id = s.snapshot().rules[0].id.clone();
        s.delete_rule(&id).unwrap();
        assert!(matches!(s.delete_rule(&id), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn persists_and_refuses_corrupt_file() {
        let dir = std::env::temp_dir().join(format!("omq-models-{}", random_hex(6)));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.yaml");
        let mut s = ModelPolicyStore::load(&path).unwrap();
        s.set_mode(Mode::Allow).unwrap();
        s.add_rule("gemma*", Action::Allow, vec![]).unwrap();
        let again = ModelPolicyStore::load(&path).unwrap();
        assert_eq!(again.snapshot(), s.snapshot());
        fs::write(&path, "nope").unwrap();
        assert!(ModelPolicyStore::load(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A hand-written file: bare hide patterns, no ids or timestamps; an old
    /// JSON file migrates on first load.
    #[test]
    fn hand_written_yaml_and_json_migration() {
        let dir = std::env::temp_dir().join(format!("omq-models-hw-{}", random_hex(6)));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("models.yaml"),
            "mode: deny\nrules:\n  - unsloth/Qwen3.8-27B-GGUF:Q4_K_XL\n  - pattern: keep-me*\n    action: allow\n",
        )
        .unwrap();
        let s = ModelPolicyStore::load(dir.join("models.yaml")).unwrap();
        let p = s.snapshot();
        assert!(!p.served("unsloth/Qwen3.8-27B-GGUF:Q4_K_XL", A));
        assert!(p.rules.iter().all(|r| !r.id.is_empty()));
        assert!(p.rules.iter().any(|r| r.action == Action::Allow));

        fs::remove_file(dir.join("models.yaml")).unwrap();
        fs::write(
            dir.join("models.json"),
            r#"{"mode":"deny","rules":[{"id":"m1","pattern":"legacy*","action":"hide","backends":[],"from_toggle":false,"created_at_ms":1}]}"#,
        )
        .unwrap();
        let s = ModelPolicyStore::load(dir.join("models.yaml")).unwrap();
        assert!(!s.snapshot().served("legacy-x", A));
        assert!(dir.join("models.yaml").exists());
        assert!(dir.join("models.json.bak").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
