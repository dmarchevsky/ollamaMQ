//! Effective settings: where each value comes from, and applying a change
//! while the proxy runs.
//!
//! Precedence, highest first: CLI flags and environment variables, the web
//! UI's overlay (`settings.json`), `appconf.yaml`, built-in defaults.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use tracing::info;

use crate::config::{AppConfig, Backend, Settings};
use crate::dispatcher::{AppState, BackendStatus};
use crate::lock::LockExt;
use crate::settings_store::SettingsStore;

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
    "clients_file",
    "models_file",
    "settings_file",
    "admin_file",
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
        request_log_path: Some("ollamamq-requests.jsonl".into()),
        request_log_max_bytes: Some(10 * 1024 * 1024),
        request_log_max_files: Some(5),
        log_content_limit: Some(65_536),
        max_body_bytes: Some(64 * 1024 * 1024),
        max_queued_bytes: Some(512 * 1024 * 1024),
        clients_file: Some(crate::clients::DEFAULT_CLIENTS_FILE.into()),
        models_file: Some(crate::model_policy::DEFAULT_MODELS_FILE.into()),
        settings_file: Some(crate::settings_store::DEFAULT_SETTINGS_FILE.into()),
        admin_file: Some(crate::admin::DEFAULT_ADMIN_FILE.into()),
        metrics_token: None,
        web_ui: None,
    }
}

/// Every source of settings, kept so the UI can show where a value comes from.
pub struct Layers {
    /// `appconf.yaml` as last read.
    pub file: AppConfig,
    /// Values given as CLI flags or environment variables.
    pub pinned: Settings,
    /// Which flag or variable pinned each of those (setting name -> label).
    pub pinned_by: BTreeMap<&'static str, String>,
    /// True when `--backend-urls` chose the backends (the list is then fixed).
    pub backends_from_cli: bool,
    /// Backends used when neither the UI nor the file lists any (the
    /// built-in `localhost:11434`), so the first UI edit starts from them.
    pub fallback_backends: Vec<Backend>,
    /// Effective settings when the proxy started, to tell which startup-only
    /// changes are still waiting for a restart.
    pub startup: Settings,
    pub store: SettingsStore,
    /// Where each store file was found (setting name -> path), for display.
    pub resolved: BTreeMap<&'static str, String>,
}

impl Default for Layers {
    fn default() -> Self {
        Self {
            file: AppConfig::default(),
            pinned: Settings::default(),
            pinned_by: BTreeMap::new(),
            backends_from_cli: false,
            fallback_backends: Vec::new(),
            startup: Settings::default(),
            store: SettingsStore::in_memory(),
            resolved: BTreeMap::new(),
        }
    }
}

impl Layers {
    /// File, then UI, then CLI/env (unset values stay `None`).
    pub fn effective(&self) -> Settings {
        self.file
            .settings
            .overlay(&self.store.overlay().settings)
            .overlay(&self.pinned)
    }

    /// The configured backend list: `--backend-urls` when given, else the
    /// UI's once edited there, else the file's, else the built-in fallback.
    pub fn backend_list(&self) -> Vec<Backend> {
        if self.backends_from_cli {
            return self.fallback_backends.clone();
        }
        match &self.store.overlay().backends {
            Some(list) => list.clone(),
            None if !self.file.backends.is_empty() => self.file.backends.clone(),
            None => self.fallback_backends.clone(),
        }
    }

    /// Where `key` gets its value: `cli`/`env` (with a label), `ui`, `file` or `default`.
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
        if set(&self.store.overlay().settings) {
            ("ui", None)
        } else if set(&self.file.settings) {
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
    state
        .allow_all_routes
        .store(eff.allow_all_routes.unwrap_or(false), Relaxed);
    state.max_queued_bytes.store(
        eff.max_queued_bytes
            .or(d.max_queued_bytes)
            .unwrap_or(512 * 1024 * 1024),
        Relaxed,
    );
    state.log_content_limit.store(
        eff.log_content_limit.map(|v| v.max(1024)).unwrap_or(65_536),
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

/// Where a store file named in the settings (`clients_file`, `models_file`,
/// `settings_file`, `admin_file`) lives.
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

    #[test]
    fn precedence_is_cli_then_ui_then_file() {
        let mut l = Layers::default();
        l.file.settings.timeout = Some(100);
        l.file.settings.stuck_timeout = Some(30);
        l.file.settings.port = Some(9000);
        let mut ui = crate::config::UiOverlay::default();
        ui.settings.timeout = Some(200);
        ui.settings.ip_identity = Some(false);
        l.store.save(ui).unwrap();
        l.pinned.port = Some(9100);
        l.pinned_by.insert("port", "--port".into());

        let e = l.effective();
        assert_eq!(e.timeout, Some(200));
        assert_eq!(e.stuck_timeout, Some(30));
        assert_eq!(e.port, Some(9100));
        assert_eq!(e.ip_identity, Some(false));
        assert_eq!(l.source("timeout"), ("ui", None));
        assert_eq!(l.source("stuck_timeout"), ("file", None));
        assert_eq!(l.source("port"), ("cli", Some("--port".into())));
        assert_eq!(l.source("max_queued_bytes"), ("default", None));
    }

    #[test]
    fn restart_pending_lists_only_startup_settings() {
        let mut l = Layers::default();
        l.startup = l.effective();
        let mut ui = crate::config::UiOverlay::default();
        ui.settings.timeout = Some(5);
        ui.settings.max_body_bytes = Some(2048);
        l.store.save(ui).unwrap();
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
    fn urls_are_normalized() {
        assert_eq!(normalize_url("10.0.0.1:8000/"), "http://10.0.0.1:8000");
        assert_eq!(normalize_url(" https://a.b "), "https://a.b");
    }
}
