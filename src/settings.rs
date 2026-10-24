//! Effective settings: where each value comes from, and applying a change
//! while the proxy runs.
//!
//! Precedence, highest first: CLI flags and environment variables,
//! `appconf.yaml`, built-in defaults. The web UI edits `appconf.yaml`
//! itself (see [`crate::config_store`]).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use tracing::info;

use crate::config::{AppConfig, Backend, Settings};
use crate::dispatcher::{AppState, BackendStatus};
use crate::lock::LockExt;

/// Settings the web UI can change, and whether a change applies at once.
pub const EDITABLE: &[(&str, bool)] = &[
    ("timeout", true),
    ("stuck_timeout", true),
    ("max_concurrent_per_backend", true),
    ("detect_backend_slots", true),
    ("max_queued_bytes", true),
    ("max_body_bytes", false),
    ("load_keep_alive", true),
    ("model_aliases", true),
    ("ip_identity", true),
    ("sticky_routing", true),
    ("sticky_ttl", true),
    ("sticky_lag", true),
    ("sticky_max_entries", true),
    ("client_priority", true),
    ("allow_all_routes", true),
    ("metrics_token", true),
    ("log_content_limit", true),
];

/// Settings shown read-only: they are read once at startup.
pub const STARTUP_ONLY: &[&str] = &[
    "host",
    "port",
    "request_log_path",
    "request_log_max_bytes",
    "request_log_max_files",
    "request_log_flush_secs",
    "clients_file",
    "models_file",
];

/// The value each setting has when nothing sets it.
pub fn defaults() -> Settings {
    Settings {
        port: Some(11435),
        host: Some("127.0.0.1".into()),
        timeout: Some(300),
        load_keep_alive: Some(86400),
        allow_all_routes: Some(false),
        stuck_timeout: Some(60),
        max_concurrent_per_backend: Some(1),
        detect_backend_slots: Some(true),
        model_aliases: Some(BTreeMap::new()),
        ip_identity: Some(true),
        sticky_routing: Some(crate::config::StickyMode::Prompt),
        sticky_ttl: Some(1800),
        sticky_lag: Some(0),
        sticky_max_entries: Some(10_000),
        client_priority: Some(crate::config::ClientPriority::Global),
        request_log_path: Some("ollamamq-requests.jsonl".into()),
        request_log_max_bytes: Some(10 * 1024 * 1024),
        request_log_max_files: Some(5),
        request_log_flush_secs: Some(5),
        log_content_limit: Some(1024 * 1024),
        max_body_bytes: Some(64 * 1024 * 1024),
        max_queued_bytes: Some(512 * 1024 * 1024),
        clients_file: Some(crate::clients::DEFAULT_CLIENTS_FILE.into()),
        models_file: Some(crate::model_policy::DEFAULT_MODELS_FILE.into()),
        metrics_token: None,
        web_ui: None,
    }
}

/// Every source of settings, kept so the UI can show where a value comes
/// from.
#[derive(Default)]
pub struct Layers {
    /// `appconf.yaml` as last read or saved.
    pub file: AppConfig,
    /// Values given as CLI flags or environment variables.
    pub pinned: Settings,
    /// Which flag or variable pinned each of those (setting name -> label).
    pub pinned_by: BTreeMap<&'static str, String>,
    /// True when `--backend-urls` chose the backends (the list is then fixed).
    pub backends_from_cli: bool,
    /// Backends used when the file lists none (the built-in
    /// `localhost:11434`), so the first UI edit starts from them.
    pub fallback_backends: Vec<Backend>,
    /// Effective settings when the proxy started, to tell which startup-only
    /// changes are still waiting for a restart.
    pub startup: Settings,
    /// Where each store file was found (setting name -> path), for display.
    pub resolved: BTreeMap<&'static str, String>,
}

impl Layers {
    /// The file, then CLI/env on top (unset values stay `None`).
    pub fn effective(&self) -> Settings {
        self.file.settings.overlay(&self.pinned)
    }

    /// The configured backend list: `--backend-urls` when given, else the
    /// file's, else the built-in fallback.
    pub fn backend_list(&self) -> Vec<Backend> {
        if self.backends_from_cli {
            return self.fallback_backends.clone();
        }
        if !self.file.backends.is_empty() {
            self.file.backends.clone()
        } else {
            self.fallback_backends.clone()
        }
    }

    /// Where `key` gets its value: `cli`/`env` (with a label), `file` or `default`.
    pub fn source(&self, key: &str) -> (&'static str, Option<String>) {
        if let Some(label) = self.pinned_by.get(key) {
            let kind = if label.starts_with("--") {
                "cli"
            } else {
                "env"
            };
            return (kind, Some(label.clone()));
        }
        let set = |s: &Settings| field(s, key).is_some_and(|v| !v.is_null());
        if set(&self.file.settings) {
            ("file", None)
        } else {
            ("default", None)
        }
    }

    /// Startup-only settings whose effective value changed since the start.
    pub fn restart_pending(&self) -> Vec<&'static str> {
        let now = defaults().overlay(&self.effective());
        let then = defaults().overlay(&self.startup);
        EDITABLE
            .iter()
            .filter(|(_, live)| !live)
            .map(|(k, _)| *k)
            .chain(STARTUP_ONLY.iter().copied())
            .filter(|k| field(&now, k) != field(&then, k))
            .collect()
    }
}

/// One setting as JSON (`None` when the key is unknown).
pub fn field(s: &Settings, key: &str) -> Option<serde_json::Value> {
    let v = serde_json::to_value(s).ok()?;
    Some(v.get(key).cloned().unwrap_or(serde_json::Value::Null))
}

/// Apply the live settings in `eff` to the running proxy.
pub fn apply(state: &AppState, eff: &Settings) {
    let d = defaults();
    let or = |v: Option<u64>, dflt: Option<u64>| v.or(dflt).unwrap_or(0);
    state.timeout.store(or(eff.timeout, d.timeout), Relaxed);
    state
        .stuck_timeout_secs
        .store(or(eff.stuck_timeout, d.stuck_timeout), Relaxed);
    state.load_keep_alive.store(
        eff.load_keep_alive.or(d.load_keep_alive).unwrap_or(86400),
        Relaxed,
    );
    // Unset: the largest per-model concurrency, so `max_concurrent_requests: N`
    // is not silently capped at 1.
    let per_backend = eff.max_concurrent_per_backend.unwrap_or_else(|| {
        state
            .model_config
            .lock_or_recover()
            .iter()
            .map(|m| m.max_concurrent_requests)
            .max()
            .unwrap_or(1)
    });
    state.max_concurrent_per_backend.store(per_backend, Relaxed);
    state
        .detect_slots
        .store(eff.detect_backend_slots.unwrap_or(true), Relaxed);
    state
        .ip_identity
        .store(eff.ip_identity.unwrap_or(true), Relaxed);
    state.sticky_mode.store(
        *eff.sticky_routing.as_ref().unwrap_or(&Default::default()) as u8,
        Relaxed,
    );
    state.sticky_ttl.store(or(eff.sticky_ttl, d.sticky_ttl), Relaxed);
    state
        .sticky_lag
        .store(eff.sticky_lag.unwrap_or(0), Relaxed);
    state.sticky_max_entries.store(
        eff.sticky_max_entries
            .or(d.sticky_max_entries)
            .unwrap_or(10_000) as u64,
        Relaxed,
    );
    state.client_priority.store(
        *eff
            .client_priority
            .as_ref()
            .unwrap_or(&crate::config::ClientPriority::Global)
            as u8,
        Relaxed,
    );
    state
        .allow_all_routes
        .store(eff.allow_all_routes.unwrap_or(false), Relaxed);
    state.max_queued_bytes.store(
        crate::config::queue_budget(
            eff.max_queued_bytes
                .or(d.max_queued_bytes)
                .unwrap_or(512 * 1024 * 1024),
        ),
        Relaxed,
    );
    state.log_content_limit.store(
        eff.log_content_limit.map(|v| v.max(1024)).unwrap_or(1024 * 1024),
        Relaxed,
    );
    *state.metrics_token.lock_or_recover() = eff.metrics_token.clone().filter(|t| !t.is_empty());
    state.set_model_aliases(eff.model_aliases.as_ref());
    // Capacity or aliases may have changed what can be dispatched.
    state.notify.notify_one();
}

/// Re-read `appconf.yaml` and apply it with the UI's changes on top: models,
/// every live setting and the backend list. Returns the number of models.
pub fn reload(state: &Arc<AppState>) -> Result<usize, String> {
    let n = crate::control::reload_model_config(state)?;
    let cfg = crate::config::load_config(&state.model_config_path)?;
    let (eff, list, fixed) = {
        let mut layers = state.config.lock_or_recover();
        layers.file = cfg;
        (
            layers.effective(),
            layers.backend_list(),
            layers.backends_from_cli,
        )
    };
    apply(state, &eff);
    if !fixed {
        apply_backends(state, &list);
    }
    Ok(n)
}

/// Where a store file named in the settings (`clients_file`, `models_file`)
/// lives.
///
/// An absolute path is used as given. A relative one is resolved from
/// `config_dir`, the folder holding `appconf.yaml`, so the whole
/// configuration can live in one folder while logs go to the working
/// directory. Older layouts kept these files in the working directory
/// (`cwd`): when the file exists there and not next to the config, that copy
/// is used, and the second value is true so the caller can say so.
pub fn store_path(name: &str, config_dir: &Path, cwd: &Path) -> (PathBuf, bool) {
    let p = Path::new(name);
    if p.is_absolute() {
        return (p.to_path_buf(), false);
    }
    let preferred = config_dir.join(p);
    let legacy = cwd.join(p);
    if !preferred.exists() && legacy.exists() && preferred != legacy {
        (legacy, true)
    } else {
        (preferred, false)
    }
}

/// The folder of the config file; `.` for a bare file name.
pub fn config_dir(config_file: &str) -> PathBuf {
    match Path::new(config_file).parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// One-time upgrade from the old side stores: a leftover `settings.json`
/// (the web UI's overlay) and `admin.json` (password changed in the UI) are
/// merged into `appconf.yaml` — the overlay outranks the file, as before —
/// written back (the original kept as `appconf.yaml.bak`) and renamed to
/// `.bak`. Does nothing when the config file itself does not exist.
pub fn merge_legacy_stores(config_file: &str, mut cfg: AppConfig) -> Result<AppConfig, String> {
    if !Path::new(config_file).exists() {
        return Ok(cfg);
    }
    let dir = config_dir(config_file);
    let mut merged = false;

    if let Some(path) = legacy_store(&dir, "settings.json") {
        #[derive(serde::Deserialize)]
        struct OldOverlay {
            #[serde(default)]
            settings: Settings,
            #[serde(default)]
            backends: Option<Vec<Backend>>,
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let old: OldOverlay = serde_json::from_str(&text)
            .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        cfg.settings = cfg.settings.overlay(&old.settings);
        if let Some(backends) = old.backends {
            cfg.backends = backends;
        }
        retire(&path)?;
        merged = true;
        info!("Migrated {} into {config_file}", path.display());
    }
    if let Some(path) = legacy_store(&dir, "admin.json") {
        #[derive(serde::Deserialize)]
        struct OldAdmin {
            username: String,
            password_hash: String,
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let old: OldAdmin = serde_json::from_str(&text)
            .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        let web = cfg.settings.web_ui.get_or_insert_with(Default::default);
        web.username = Some(old.username);
        web.password_hash = Some(old.password_hash);
        retire(&path)?;
        merged = true;
        info!("Migrated {} into {config_file}", path.display());
    }

    if merged {
        // An old overlay could hold values that never passed today's checks;
        // failing loudly beats running on quietly broken settings.
        cfg.settings.validate().map_err(|e| format!("merged config is invalid: {e}"))?;
        crate::config_store::save(Path::new(config_file), &cfg)?;
    }
    Ok(cfg)
}

/// A legacy store file, resolved like every store file (config folder first,
/// then the working directory).
fn legacy_store(config_dir: &Path, name: &str) -> Option<PathBuf> {
    let (path, _) = store_path(name, config_dir, Path::new(""));
    path.exists().then_some(path)
}

fn retire(path: &Path) -> Result<(), String> {
    let mut bak = path.as_os_str().to_owned();
    bak.push(".bak");
    std::fs::rename(path, PathBuf::from(bak))
        .map_err(|e| format!("cannot rename {}: {e}", path.display()))
}

/// `http://host:port`, no trailing slash; `http://` added when missing.
pub fn normalize_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

/// Make the running backends match `list`: new URLs are added, existing ones
/// get their name, token, kind and enabled state. Backends missing from the
/// list are turned off until the next restart drops them (requests in flight
/// refer to backends by position, so none can be removed while running).
pub fn apply_backends(state: &Arc<AppState>, list: &[Backend]) {
    let models = state.model_config.lock_or_recover().clone();
    let mut backends = state.backends.lock_or_recover();
    let wanted: HashSet<String> = list.iter().map(|b| normalize_url(&b.url)).collect();
    for entry in list {
        let url = normalize_url(&entry.url);
        let idx = match backends.iter().position(|b| b.url == url) {
            Some(i) => i,
            None => {
                let declared =
                    crate::control::declared_models_for(std::slice::from_ref(&url), &models);
                info!("Backend {} added", url);
                backends.push(BackendStatus::new(
                    url.clone(),
                    None,
                    declared.into_iter().next().unwrap_or_default(),
                ));
                backends.len() - 1
            }
        };
        let b = &mut backends[idx];
        b.name = entry
            .name
            .clone()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());
        b.token = entry.token.clone().filter(|t| !t.is_empty());
        b.redact_log = entry.redact_log;
        if let Some(pinned) = crate::dispatcher::ApiSet::from_kind(entry.kind.as_deref()) {
            b.forced_api = pinned;
            b.api_type = pinned;
        } else if b.forced_api != crate::dispatcher::ApiSet::EMPTY {
            // Pin removed: let the next probe classify it.
            b.forced_api = crate::dispatcher::ApiSet::EMPTY;
        }
        set_disabled(b, entry.disabled);
    }
    for b in backends.iter_mut().filter(|b| !wanted.contains(&b.url)) {
        set_disabled(b, true);
    }
    drop(backends);
    crate::dispatcher::clear_affinity(state);
    crate::dispatcher::clear_listing_cache(state);
    state.notify.notify_one();
}

fn set_disabled(b: &mut BackendStatus, disabled: bool) {
    if disabled && !b.disabled {
        info!("Backend {} disabled", b.url);
        b.is_online = false;
    } else if !disabled && b.disabled {
        info!("Backend {} enabled; probing it", b.url);
    }
    b.disabled = disabled;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A crash should cost the last few seconds of a conversation, not the last
    /// minute — and the default has to stay inside `validate()`'s floor.
    #[test]
    fn the_conversation_log_flushes_after_five_seconds_by_default() {
        assert_eq!(defaults().request_log_flush_secs, Some(5));
        defaults().validate().expect("the defaults validate");
    }

    /// A backend's capture switch follows the file, in both directions.
    #[test]
    fn backends_apply_carries_the_capture_switch() {
        let state = Arc::new(AppState::new(
            vec![("http://a:1".to_string(), None)],
            5,
            86400,
            60,
            1,
            "appconf.yaml".into(),
            Vec::new(),
            crate::convlog::ConvLogger::disabled(),
            65_536,
            512 * 1024 * 1024,
        ));
        let list = [Backend {
            url: "http://a:1".into(),
            name: None,
            disabled: false,
            token: None,
            kind: None,
            redact_log: true,
        }];
        apply_backends(&state, &list);
        assert!(state.backends.lock_or_recover()[0].redact_log);
        apply_backends(
            &state,
            &[Backend {
                redact_log: false,
                ..list[0].clone()
            }],
        );
        assert!(!state.backends.lock_or_recover()[0].redact_log);
    }

    #[test]
    fn precedence_is_cli_then_file() {
        let mut l = Layers::default();
        l.file.settings.timeout = Some(100);
        l.file.settings.stuck_timeout = Some(30);
        l.file.settings.port = Some(9000);
        l.pinned.port = Some(9100);
        l.pinned_by.insert("port", "--port".into());

        let e = l.effective();
        assert_eq!(e.timeout, Some(100));
        assert_eq!(e.stuck_timeout, Some(30));
        assert_eq!(e.port, Some(9100));
        assert_eq!(l.source("timeout"), ("file", None));
        assert_eq!(l.source("stuck_timeout"), ("file", None));
        assert_eq!(l.source("port"), ("cli", Some("--port".into())));
        assert_eq!(l.source("max_queued_bytes"), ("default", None));
    }

    #[test]
    fn restart_pending_lists_only_startup_settings() {
        let mut l = Layers::default();
        l.startup = l.effective();
        l.file.settings.timeout = Some(5);
        l.file.settings.max_body_bytes = Some(2048);
        assert_eq!(l.restart_pending(), vec!["max_body_bytes"]);
    }

    #[test]
    fn store_files_live_next_to_the_config() {
        let root = std::env::temp_dir().join(format!("omq-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (cfg, cwd) = (root.join("config"), root.join("data"));
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        // New setup: nothing anywhere yet -> next to the config.
        assert_eq!(
            store_path("clients.json", &cfg, &cwd),
            (cfg.join("clients.json"), false)
        );
        // Old layout: the file is only in the working directory -> keep using it.
        std::fs::write(cwd.join("clients.json"), "{}").unwrap();
        assert_eq!(
            store_path("clients.json", &cfg, &cwd),
            (cwd.join("clients.json"), true)
        );
        // Moved next to the config: that copy wins.
        std::fs::write(cfg.join("clients.json"), "{}").unwrap();
        assert_eq!(
            store_path("clients.json", &cfg, &cwd),
            (cfg.join("clients.json"), false)
        );
        // Absolute paths are taken as given.
        assert_eq!(
            store_path("/x/y.json", &cfg, &cwd),
            (PathBuf::from("/x/y.json"), false)
        );
        // Config in the working directory: one place, never "legacy".
        assert_eq!(
            store_path("clients.json", &cwd, &cwd),
            (cwd.join("clients.json"), false)
        );
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(config_dir("/config/appconf.yaml"), PathBuf::from("/config"));
        assert_eq!(config_dir("appconf.yaml"), PathBuf::from("."));
        assert_eq!(config_dir("conf/app.yaml"), PathBuf::from("conf"));
    }

    #[test]
    fn legacy_stores_merge_into_appconf() {
        let root = std::env::temp_dir().join(format!("omq-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let conf = root.join("appconf.yaml");
        std::fs::write(
            &conf,
            "settings:\n  timeout: 300\nbackends:\n  - http://a:1\n",
        )
        .unwrap();
        std::fs::write(
            root.join("settings.json"),
            r#"{"settings":{"timeout":100,"allow_all_routes":true},"backends":[{"url":"http://b:2","name":"B"}]}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("admin.json"),
            r#"{"username":"boss","password_hash":"$argon2id$v=19$m=1,t=1,p=1$c2FsdA$hash","changed_at_ms":1}"#,
        )
        .unwrap();

        let cfg = merge_legacy_stores(conf.to_str().unwrap(), crate::config::load_config(conf.to_str().unwrap()).unwrap())
            .unwrap();
        // The old overlay outranked the file, so its values survive.
        assert_eq!(cfg.settings.timeout, Some(100));
        assert_eq!(cfg.settings.allow_all_routes, Some(true));
        assert_eq!(cfg.backends.len(), 1);
        assert_eq!(cfg.backends[0].url, "http://b:2");
        let web = cfg.settings.web_ui.clone().unwrap();
        assert_eq!(web.username.as_deref(), Some("boss"));
        assert!(web.password_hash.is_some());

        // The merged result is on disk, the originals retired, the original
        // file kept as the undo backup.
        let on_disk = crate::config::load_config(conf.to_str().unwrap()).unwrap();
        assert_eq!(on_disk.settings.timeout, Some(100));
        assert!(!root.join("settings.json").exists() && root.join("settings.json.bak").exists());
        assert!(!root.join("admin.json").exists() && root.join("admin.json.bak").exists());
        assert!(root.join("appconf.yaml.bak").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn urls_are_normalized() {
        assert_eq!(normalize_url("10.0.0.1:8000/"), "http://10.0.0.1:8000");
        assert_eq!(normalize_url(" https://a.b "), "https://a.b");
    }
}
