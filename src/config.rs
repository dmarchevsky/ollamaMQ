//! Central configuration file (`appconf.yaml`, override with
//! `-c/--model-config`). Three sections:
//!
//! * `backends` — backend URLs ollamaMQ connects to (same as
//!   `--backend-urls`; CLI wins when given).
//! * `settings` — runtime settings; see [`Settings`] for the full list and
//!   the default each one falls back to. CLI flags win when given.
//! * `models` — models to load onto backends via the model-control
//!   load/unload logic, with per-model parameters.
//!
//! ```yaml
//! backends:
//!   - http://10.137.1.1:11434          # plain URL — no auth
//!   - url: http://10.137.1.2:11434     # object form with an optional token,
//!     token: "sekrit"                  # sent as `Authorization: Bearer <token>`
//!
//! settings:
//!   port: 11435
//!   timeout: 300
//!   load_keep_alive: 86400
//!
//! models:
//!   - name: "gpt-oss:120b"
//!     identifier: "my-gpt"
//!     max_ctx: 128000
//!     max_concurrent_requests: 3
//!     backends:
//!       - http://10.137.1.1:11434
//! ```

use serde::{Deserialize, Serialize};
use std::fs;

pub const DEFAULT_CONFIG_FILE: &str = "appconf.yaml";

/// What identifies a conversation for sticky routing (see
/// [`Settings::sticky_routing`]). Stored as a small integer on `AppState` so
/// the scheduler can read it without a lock, like the other live settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StickyMode {
    /// Today's behavior: every request picks the least-loaded backend.
    Off,
    /// Only an explicit session id (`X-Session-Id` header or a body
    /// `prompt_cache_key`) makes a conversation.
    Header,
    /// Plus a fingerprint of the prompt, so chat clients that resend their
    /// whole history get affinity without sending anything (default).
    #[default]
    Prompt,
    /// Plus the client identity itself: each user gets one home backend. Right
    /// for one-session-per-user setups, wrong for a client running several
    /// conversations at once.
    Identity,
}

impl StickyMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => StickyMode::Off,
            1 => StickyMode::Header,
            3 => StickyMode::Identity,
            _ => StickyMode::Prompt,
        }
    }
}

/// How far a client's self-declared session priority reaches. See
/// `Settings::client_priority`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientPriority {
    /// Priority is operator-set only; the header is ignored.
    Off,
    /// `high` is honoured inside its own client; `top` is treated as `high`.
    /// (`self` is a Rust keyword, hence the rename.)
    #[serde(rename = "self")]
    Intra,
    /// Both `high` and `top` are honoured.
    #[default]
    Global,
}

impl ClientPriority {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => ClientPriority::Off,
            1 => ClientPriority::Intra,
            _ => ClientPriority::Global,
        }
    }
}

/// Runtime settings (all optional; CLI flags take precedence).
///
/// The web UI edits these in place: a save rewrites `appconf.yaml` itself
/// (the previous version is kept as `appconf.yaml.bak`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Port to listen on (default 11435).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Host/interface to bind to (default 127.0.0.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Request timeout in seconds (default 300).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// `keep_alive` seconds sent with model-control loads (default 86400).
    /// `-1` keeps the model loaded indefinitely (Ollama semantics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_keep_alive: Option<i64>,
    /// Enable fallback proxy for non-standard endpoints (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_all_routes: Option<bool>,
    /// How long (seconds) a request may wait when no backend can ever serve
    /// it before answering 503 (default 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck_timeout: Option<u64>,
    /// Max simultaneous in-flight requests per backend (default 1 — the
    /// historical one-request-per-backend behavior). Per-model sub-limits come
    /// from each model entry's `max_concurrent_requests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_per_backend: Option<u32>,
    /// Use the parallel slots a backend reports (llama.cpp `/props`
    /// `total_slots`, halogen `/health` `slots`) as its capacity instead of
    /// `max_concurrent_per_backend` (default true). Models without their own
    /// `max_concurrent_requests` may then use all of those slots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_backend_slots: Option<bool>,
    /// Model name aliases: a request for the key (any case) is served as the
    /// value, e.g. `codex-auto-review: Qwen3.8-Flash-Next`. Applied before
    /// routing, the model policy and metrics; reloaded with the config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_aliases: Option<std::collections::BTreeMap<String, String>>,
    /// Identify requests that carry neither a client key nor `X-User-ID` by
    /// their address, as `ip-<address>` (default true). False: they all
    /// share the `anonymous` queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_identity: Option<bool>,
    /// Keep a conversation on the backend that served its previous turn, so a
    /// multi-turn prompt is not re-prefilled elsewhere (default `prompt`).
    /// `off` disables it, `header` honors only an explicit session id, `prompt`
    /// also fingerprints the conversation's first turn, `identity` pins a whole
    /// client id to one backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky_routing: Option<StickyMode>,
    /// How long a conversation may be idle and still keep its backend, in
    /// seconds (default 1800). Raise it towards `load_keep_alive` when contexts
    /// are meant to stay resident for hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky_ttl: Option<u64>,
    /// How many more requests a sticky backend may be running than the
    /// least-loaded eligible one before the conversation moves anyway
    /// (default 0: only a backend with free capacity keeps its session).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky_lag: Option<u64>,
    /// Conversations remembered at once; the idlest are dropped past it
    /// (default 10000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky_max_entries: Option<usize>,
    /// How far a `X-Session-Priority` a client declares for itself reaches
    /// (default `global`). `off`: the header is ignored and priority is
    /// operator-only. `self`: `high` (first within its own client) is honoured,
    /// `top` is treated as `high`. `global`: both are honoured, so a client can
    /// put its session first on the whole board. Either way the header is
    /// honoured only for authenticated clients, and an operator pin always
    /// wins over what the client claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_priority: Option<ClientPriority>,
    /// Path of the size-rotated request log file. Relative paths (and the
    /// default "ollamamq-requests.jsonl") resolve from the folder holding
    /// `appconf.yaml`, like every other state file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_log_path: Option<String>,
    /// Max bytes per request-log file before rotation (default 10 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_log_max_bytes: Option<u64>,
    /// Number of rotated request-log files to keep (default 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_log_max_files: Option<usize>,
    /// Seconds a conversation must be quiet for before it is written to the log
    /// (default 60). A conversation that resumes is written again as one longer
    /// line, so a smaller value costs file size and a larger one delays the
    /// record of a finished conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_log_flush_secs: Option<u64>,
    /// Max bytes of request/response body content captured per record in the
    /// request log; values below 1024 are raised to 1024 (default 1048576).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_content_limit: Option<usize>,
    /// How far back the Activity history behind the Overview page reaches, in
    /// hours (default 24). It is the longest window the Overview offers, the
    /// age at which finished-request records fall out of memory, and the age
    /// at which journaled records are dropped when read back after a restart.
    /// Records are journalled next to the request log, so the history now
    /// survives restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_hours: Option<u64>,
    /// Cap on finished-request records kept in memory for the Overview,
    /// whatever their age (default 50000). Each record is a few hundred bytes;
    /// raise it past your busiest 2 × `history_hours` of requests so the cap
    /// never trims inside the retention window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_max_records: Option<usize>,
    /// Largest request body accepted on normal routes, in bytes (default
    /// 64 MiB). Bodies are buffered whole — they have to be, since a request
    /// may wait in a queue before a backend takes it — so this is the per
    /// request memory commitment. `/api/blobs/{digest}` keeps its own 1 GiB
    /// allowance for model-layer uploads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_bytes: Option<usize>,
    /// Max total bytes of request bodies waiting in all queues at once
    /// (default 512 MiB). Past this the proxy answers 503 instead of
    /// accumulating; a request always gets in when the queues are empty, so
    /// one oversized body can never deadlock the proxy against itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_queued_bytes: Option<u64>,
    /// Where clients and their API keys are stored (default "clients.yaml").
    /// Managed from the web UI. Like the other store files, a relative path
    /// is resolved from the folder holding this config file (see
    /// `settings::store_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients_file: Option<String>,
    /// Where the model filter rules are stored (default "models.yaml").
    /// Managed from the web UI's Models page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_file: Option<String>,
    /// Bearer token required by `GET /metrics`. Unset (the default) leaves
    /// the endpoint open, like `/health`. `OLLAMA_MQ_METRICS_TOKEN` overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics_token: Option<String>,
    /// Web UI at `/ui`, behind HTTP basic auth. Disabled unless a password is
    /// set here or in `OLLAMA_MQ_UI_PASSWORD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_ui: Option<WebUiSettings>,
}

/// `settings.web_ui`. The environment variables `OLLAMA_MQ_UI_USER` and
/// `OLLAMA_MQ_UI_PASSWORD` override these values.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WebUiSettings {
    /// Basic-auth user name (default "admin").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Basic-auth password. No password and no hash = the web UI is off.
    /// The UI's "Change password" writes `password_hash` instead, so the
    /// plaintext is only ever the first-run password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// argon2id PHC string (`$argon2id$v=19$...`). Wins over `password` and
    /// over `OLLAMA_MQ_UI_PASSWORD`; recovery: delete it and restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
}

/// One backend entry from the `backends:` list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Backend {
    /// The backend's base URL (scheme included).
    pub url: String,
    /// Display name shown in the UI instead of `host:port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Kept in the list but never probed or routed to.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    /// Optional API key sent to this backend as
    /// `Authorization: Bearer <token>` on every request the proxy makes to
    /// it — proxied inference, model listings and metadata reads, health
    /// probes, and model load/unload control ops. A client's own auth header
    /// is never overridden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Pin this backend's API family instead of waiting for the health probe to
    /// classify it. `systemone` is the only meaningful value today: Laya-style
    /// decision-model servers expose no model-listing endpoint at all, so no
    /// probe can positively identify them, and an unclassified family makes
    /// every request to that backend unroutable. A pinned family is fixed for
    /// the process lifetime — probes never talk it out of existence.
    ///
    /// Absent, empty or unrecognised = classify from probes (the behaviour
    /// every existing config relies on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Keep the log entry but store no response text for traffic served by this
    /// backend: status, size, timing and the rest of the record stay as they
    /// were. A request's own text is logged before routing, so it is covered by
    /// the client/key setting, not by this one.
    #[serde(default, skip_serializing_if = "is_false")]
    pub redact_log: bool,
}

/// YAML shape of one `backends:` entry: either a bare URL string or an object
/// with `url` and optional `token`/`kind`. Both forms are accepted so existing
/// configs keep working unchanged.
#[derive(Deserialize)]
#[serde(untagged)]
enum BackendEntrySpec {
    Url(String),
    Full(Backend),
}

fn de_backends<'de, D>(d: D) -> Result<Vec<Backend>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let specs = Vec::<BackendEntrySpec>::deserialize(d)?;
    Ok(specs
        .into_iter()
        .map(|spec| match spec {
            BackendEntrySpec::Url(url) => Backend {
                url,
                name: None,
                disabled: false,
                token: None,
                kind: None,
                redact_log: false,
            },
            BackendEntrySpec::Full(b) => b,
        })
        .collect())
}

/// Top-level config file structure.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// Backends to connect to (CLI `--backend-urls` wins when given).
    #[serde(
        default,
        deserialize_with = "de_backends",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub backends: Vec<Backend>,
    /// Runtime settings.
    #[serde(default)]
    pub settings: Settings,
    /// Models to load onto backends.
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

/// One model entry from `appconf.yaml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Model name as the backend knows it.
    pub name: String,
    /// Label attached to the load operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// Max context window (Ollama `num_ctx` / LM Studio `context_length`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ctx: Option<u64>,
    /// `keep_alive` override in seconds (Ollama loads). `-1` keeps the
    /// model loaded indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<i64>,
    /// Max simultaneous in-flight requests for this model on one backend
    /// (default 1). Enforced by the scheduler; also bounded by the global
    /// `settings.max_concurrent_per_backend` cap.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub max_concurrent_requests: u32,
    /// Backend URLs to load this model on. Empty = any suitable backend.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<String>,
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Bytes this process may grow to, as its control group says. Both cgroup
/// layouts are checked, because a container gets whichever the host runs:
/// v2 `memory.max` holds `max` when unlimited, v1
/// `memory/memory.limit_in_bytes` holds an enormous sentinel instead. `None`
/// means "nothing bounds us here" — a bare host, or an unreadable cgroup.
pub fn memory_limit() -> Option<u64> {
    let v2 = std::fs::read_to_string("/sys/fs/cgroup/memory.max");
    if let Ok(text) = v2 {
        return parse_limit(&text);
    }
    let v1 = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes");
    match v1 {
        Ok(text) => parse_limit(&text),
        Err(_) => None,
    }
}

fn parse_limit(text: &str) -> Option<u64> {
    let text = text.trim();
    if text == "max" {
        return None;
    }
    // v1 writes something near u64::MAX for "no limit"; anything that big is
    // not a limit anyone set on purpose.
    let bytes = text.parse::<u128>().ok()?;
    if bytes >= 1 << 62 {
        return None;
    }
    Some(bytes as u64)
}

/// What the request queue is actually allowed to hold. A queue may never be
/// sized so that filling it is itself the out-of-memory event: the whole point
/// of `max_queued_bytes` is to shed load instead of absorbing it, and a limit
/// above half the container's memory does the opposite — the bodies it keeps
/// resident are what gets killed. So the ceiling is halved when it would
/// otherwise eat more than half of what the box has, and the operator hears
/// about it rather than finding out from a restart loop.
pub fn queue_budget(asked: u64) -> u64 {
    let limit = memory_limit();
    let budget = clamp_queue_budget(asked, limit);
    if budget != asked {
        tracing::warn!(
            "settings.max_queued_bytes is {} B but this process may only grow to {} B; \
             the queue ceiling is {} B (half of it) so a flood is shed, not absorbed",
            asked,
            limit.unwrap_or(0),
            budget
        );
    }
    budget
}

/// The pure half of [`queue_budget`], so the rule can be tested without a
/// cgroup to argue with.
pub fn clamp_queue_budget(asked: u64, limit: Option<u64>) -> u64 {
    match limit {
        Some(limit) if asked > limit / 2 => limit / 2,
        _ => asked,
    }
}

impl Settings {
    /// `self` with every value `top` sets replacing this one's.
    pub fn overlay(&self, top: &Settings) -> Settings {
        macro_rules! pick {
            ($($f:ident),*) => { Settings { $($f: top.$f.clone().or_else(|| self.$f.clone()),)* } };
        }
        pick!(
            port, host, timeout, load_keep_alive, allow_all_routes, stuck_timeout,
            max_concurrent_per_backend, detect_backend_slots, model_aliases, ip_identity,
            sticky_routing, sticky_ttl, sticky_lag, sticky_max_entries, client_priority,
            request_log_path, request_log_max_bytes, request_log_max_files,
            request_log_flush_secs, log_content_limit,
            history_hours, history_max_records,
            max_body_bytes, max_queued_bytes, clients_file, models_file,
            metrics_token, web_ui
        )
    }

    /// Values a person could set that would break the proxy.
    pub fn validate(&self) -> Result<(), String> {
        let at_least = |v: Option<u64>, min: u64, what: &str| match v {
            Some(v) if v < min => Err(format!("{what} must be at least {min}")),
            _ => Ok(()),
        };
        at_least(self.timeout, 1, "timeout")?;
        at_least(self.stuck_timeout, 1, "stuck_timeout")?;
        at_least(self.max_concurrent_per_backend.map(u64::from), 1, "max_concurrent_per_backend")?;
        at_least(self.max_queued_bytes, 1024 * 1024, "max_queued_bytes")?;
        at_least(self.max_body_bytes.map(|v| v as u64), 1024, "max_body_bytes")?;
        at_least(self.log_content_limit.map(|v| v as u64), 1024, "log_content_limit")?;
        at_least(self.history_hours, 1, "history_hours")?;
        at_least(
            self.history_max_records.map(|v| v as u64),
            100,
            "history_max_records",
        )?;
        at_least(self.request_log_flush_secs, 5, "request_log_flush_secs")?;
        at_least(self.sticky_ttl, 1, "sticky_ttl")?;
        at_least(self.sticky_max_entries.map(|v| v as u64), 1, "sticky_max_entries")?;
        if self.load_keep_alive.is_some_and(|v| v < -1) {
            return Err("load_keep_alive must be -1 (forever) or more".into());
        }
        if let Some(aliases) = &self.model_aliases {
            for (k, v) in aliases {
                if k.trim().is_empty() || v.trim().is_empty() {
                    return Err("model aliases need both a name and a model".into());
                }
            }
        }
        Ok(())
    }
}

/// Checks a backend list: http(s) URLs, no duplicates, names unique and short.
pub fn validate_backends(list: &[Backend]) -> Result<(), String> {
    let mut urls = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    for b in list {
        let url = b.url.trim().trim_end_matches('/').to_lowercase();
        if !(url.starts_with("http://") || url.starts_with("https://")) || url.len() < 10 {
            return Err(format!("'{}' is not an http:// or https:// URL", b.url));
        }
        if !urls.insert(url) {
            return Err(format!("backend {} is listed twice", b.url));
        }
        if let Some(n) = &b.name {
            let n = n.trim();
            if n.is_empty() || n.chars().count() > 40 {
                return Err("backend names are 1–40 characters".into());
            }
            if !names.insert(n.to_lowercase()) {
                return Err(format!("two backends are named '{n}'"));
            }
        }
    }
    Ok(())
}

fn one() -> u32 {
    1
}

fn is_one(v: &u32) -> bool {
    *v == 1
}

/// Load and parse the config file. Returns an error string suitable for
/// user-facing display (missing file, invalid YAML, etc.). Validates that
/// keep-alive values are `-1` (keep loaded indefinitely) or non-negative.
pub fn load_config(path: &str) -> Result<AppConfig, String> {
    let content =
        fs::read_to_string(path).map_err(|e| format!("cannot read '{}': {}", path, e))?;
    let cfg: AppConfig =
        serde_yaml::from_str(&content).map_err(|e| format!("invalid YAML in '{}': {}", path, e))?;

    let check = |v: i64, what: &str| -> Result<(), String> {
        if v < -1 {
            Err(format!(
                "invalid {} in '{}': {} (use -1 to keep the model loaded indefinitely)",
                what, path, v
            ))
        } else {
            Ok(())
        }
    };
    if let Some(v) = cfg.settings.load_keep_alive {
        check(v, "settings.load_keep_alive")?;
    }
    for m in &cfg.models {
        if let Some(v) = m.keep_alive {
            check(v, &format!("keep_alive of model '{}'", m.name))?;
        }
    }

    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let yaml = r#"
backends:
  - http://10.137.1.1:11434
  - http://10.137.1.2:11434

settings:
  port: 11500
  host: 0.0.0.0
  timeout: 120
  load_keep_alive: 3600
  allow_all_routes: true

models:
  - name: "gpt-oss:120b"
    identifier: "my-gpt"
    max_ctx: 128000
    keep_alive: 3600
    max_concurrent_requests: 3
    backends:
      - http://10.137.1.1:11434
"#;
        let tmp = std::env::temp_dir().join("ollamamq_test_full.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert_eq!(cfg.backends.len(), 2);
        assert_eq!(cfg.settings.port, Some(11500));
        assert_eq!(cfg.settings.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(cfg.settings.timeout, Some(120));
        assert_eq!(cfg.settings.load_keep_alive, Some(3600));
        assert_eq!(cfg.settings.allow_all_routes, Some(true));
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].name, "gpt-oss:120b");
        assert_eq!(cfg.models[0].identifier.as_deref(), Some("my-gpt"));
        assert_eq!(cfg.models[0].max_ctx, Some(128000));
        assert_eq!(cfg.models[0].keep_alive, Some(3600));
        assert_eq!(cfg.models[0].max_concurrent_requests, 3);
        assert_eq!(cfg.models[0].backends.len(), 1);
    }

    #[test]
    fn parse_backends_plain_and_token_forms() {
        let yaml = r#"
backends:
  - http://10.137.1.1:11434
  - url: http://10.137.1.2:11434
    token: "sekrit"
  - url: http://10.137.1.5:1234
"#;
        let tmp = std::env::temp_dir().join("ollamamq_test_backends.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert_eq!(cfg.backends.len(), 3);
        // Bare string: no token.
        assert_eq!(cfg.backends[0].url, "http://10.137.1.1:11434");
        assert_eq!(cfg.backends[0].token, None);
        // Object with a token.
        assert_eq!(cfg.backends[1].url, "http://10.137.1.2:11434");
        assert_eq!(cfg.backends[1].token.as_deref(), Some("sekrit"));
        // Object without a token is valid too.
        assert_eq!(cfg.backends[2].url, "http://10.137.1.5:1234");
        assert_eq!(cfg.backends[2].token, None);
    }

    /// `kind:` pins a backend's API family — needed because Laya answers no
    /// endpoint that identifies it. Bare-string entries and object entries
    /// without the key keep parsing exactly as before, and an unrecognised
    /// value parses (the caller ignores it) rather than rejecting the file.
    #[test]
    fn parse_backend_kind_key() {
        let yaml = r#"
backends:
  - http://127.0.0.1:11434
  - url: http://127.0.0.1:8772
    kind: systemone
  - url: http://127.0.0.1:8700
    kind: "who-knows"
"#;
        let tmp = std::env::temp_dir().join("ollamamq_test_kind.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert_eq!(cfg.backends.len(), 3);
        // Bare string and object-without-kind: unchanged behaviour.
        assert_eq!(cfg.backends[0].kind, None);
        assert_eq!(cfg.backends[2].kind.as_deref(), Some("who-knows"));
        assert_eq!(cfg.backends[1].kind.as_deref(), Some("systemone"));
        // The key is only serialised when set, so configs round-trip unchanged.
        assert!(
            !serde_yaml::to_string(&cfg.backends[0])
                .unwrap()
                .contains("kind")
        );
    }

    /// `redact_log: true` keeps the log entry and drops its response text; a
    /// bare URL entry and an object without the key both keep capture on, and
    /// the key is not serialised when it is off (config files stay unchanged).
    #[test]
    fn parse_backend_redact_log_key() {
        let yaml = r#"
backends:
  - http://127.0.0.1:11434
  - url: http://127.0.0.1:8000
    redact_log: true
"#;
        let tmp = std::env::temp_dir().join("ollamamq_test_redact.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert!(!cfg.backends[0].redact_log);
        assert!(cfg.backends[1].redact_log);
        assert!(!serde_yaml::to_string(&cfg.backends[0]).unwrap().contains("redact_log"));
        assert!(serde_yaml::to_string(&cfg.backends[1]).unwrap().contains("redact_log"));
    }

    #[test]
    fn backend_object_without_url_is_rejected() {
        let yaml = "backends:\n  - token: \"sekrit\"\n";
        let tmp = std::env::temp_dir().join("ollamamq_test_backend_bad.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        assert!(load_config(tmp.to_str().unwrap()).is_err());
    }

    #[test]
    fn parse_minimal_config_defaults() {
        let yaml = "models:\n  - name: llama3\n";
        let tmp = std::env::temp_dir().join("ollamamq_test_min.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert!(cfg.backends.is_empty());
        assert_eq!(cfg.settings.port, None); // built-in default applies
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].name, "llama3");
        assert_eq!(cfg.models[0].max_concurrent_requests, 1); // default
        assert!(cfg.models[0].backends.is_empty()); // any backend
        assert!(cfg.models[0].max_ctx.is_none());
        assert!(cfg.models[0].identifier.is_none());
    }

    #[test]
    fn empty_file_is_valid() {
        let tmp = std::env::temp_dir().join("ollamamq_test_empty.yaml");
        std::fs::write(&tmp, "").unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert!(cfg.backends.is_empty());
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn keep_alive_minus_one_is_infinite() {
        let yaml = r#"
settings:
  load_keep_alive: -1
models:
  - name: llama3
    keep_alive: -1
"#;
        let tmp = std::env::temp_dir().join("ollamamq_test_ka_neg.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert_eq!(cfg.settings.load_keep_alive, Some(-1));
        assert_eq!(cfg.models[0].keep_alive, Some(-1));
    }

    #[test]
    fn keep_alive_below_minus_one_is_rejected() {
        let yaml = "models:\n  - name: llama3\n    keep_alive: -5\n";
        let tmp = std::env::temp_dir().join("ollamamq_test_ka_bad.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        assert!(load_config(tmp.to_str().unwrap())
            .err()
            .unwrap()
            .contains("-5"));
    }

    /// The queue may not be sized so that filling it is the out-of-memory event.
    #[test]
    fn a_queue_bigger_than_the_box_is_halved() {
        let mib = 1024 * 1024;
        let gib = 1024 * mib;
        // The shipped pair: 512 MiB of queue inside a 1 GiB container — exactly
        // half, so nothing is lowered.
        assert_eq!(clamp_queue_budget(512 * mib, Some(gib)), 512 * mib);
        // Anything above half gives way to half.
        assert_eq!(clamp_queue_budget(gib, Some(gib)), gib / 2);
        assert_eq!(clamp_queue_budget(2 * gib, Some(3 * gib / 4)), 3 * gib / 8);
        // A small ask in a small box is left alone (it is already under half).
        assert_eq!(clamp_queue_budget(64 * mib, Some(gib)), 64 * mib);
        // No limit: the setting stands, since nothing bounds the process.
        assert_eq!(clamp_queue_budget(4 * gib, None), 4 * gib);
    }

    /// `max` in v2 and a sentinel near `u64::MAX` in v1 both mean unlimited.
    #[test]
    fn an_unlimited_cgroup_is_not_a_memory_limit() {
        assert_eq!(parse_limit("max\n"), None);
        assert_eq!(parse_limit("9223372036854771712\n"), None);
        assert_eq!(parse_limit("1073741824\n"), Some(1073741824));
        assert_eq!(parse_limit("  268435456 "), Some(268435456));
        assert_eq!(parse_limit("not a number"), None);
        assert_eq!(parse_limit(""), None);
    }

    #[test]
    fn missing_file_is_error() {
        assert!(load_config("/nonexistent/appconf.yaml").is_err());
    }
}
