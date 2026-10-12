//! `/ui/api/settings`: view and change the configuration from the web UI.
//!
//! Changes are saved in the settings overlay (`settings.json`) and applied
//! at once where the proxy allows it; see [`crate::settings`].

use std::collections::HashSet;
use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{ApiError, ApiResult};
use crate::config::{Backend, Settings, UiOverlay};
use crate::dispatcher::AppState;
use crate::lock::LockExt;
use crate::metrics::backend_label;
use crate::settings::{self, EDITABLE, STARTUP_ONLY, defaults, field, normalize_url};

/// Shown instead of a secret's value.
const MASK: &str = "••••••••";
const SECRET: &[&str] = &["metrics_token"];

#[derive(Serialize)]
pub struct SettingsView {
    fields: Vec<FieldView>,
    startup: Vec<FieldView>,
    /// Startup-only settings changed since the proxy started.
    restart_pending: Vec<&'static str>,
    can_undo: bool,
    backends: Vec<BackendCfgView>,
    /// Set when `--backend-urls` fixed the list (it cannot be edited).
    backends_locked: Option<String>,
}

#[derive(Serialize)]
struct FieldView {
    key: &'static str,
    value: Value,
    /// `default`, `file` (appconf.yaml), `ui`, `cli` or `env`.
    source: &'static str,
    /// The flag or variable that pins it; the UI cannot change it then.
    locked_by: Option<String>,
    /// Applies without a restart.
    live: bool,
    /// What it falls back to when the UI value is reset.
    fallback: Value,
    /// Where that fallback comes from: `file` or `default`.
    fallback_source: &'static str,
}

#[derive(Serialize)]
struct BackendCfgView {
    url: String,
    label: String,
    name: Option<String>,
    token_set: bool,
    kind: Option<String>,
    disabled: bool,
    /// No longer configured; it stays (disabled) until the next restart.
    removed: bool,
    online: bool,
    api_type: &'static str,
    slots: u32,
    slots_detected: bool,
}

fn mask(key: &str, v: Value) -> Value {
    if SECRET.contains(&key) && !v.is_null() {
        Value::String(MASK.into())
    } else {
        v
    }
}

pub fn view(state: &AppState) -> SettingsView {
    let layers = state.config.lock_or_recover();
    let full = defaults().overlay(&layers.effective());
    let below_ui = defaults()
        .overlay(&layers.file.settings)
        .overlay(&layers.pinned);
    let make = |key: &'static str, live: bool| {
        let (source, locked_by) = layers.source(key);
        FieldView {
            key,
            value: mask(key, field(&full, key).unwrap_or_default()),
            source,
            locked_by,
            live,
            fallback: mask(key, field(&below_ui, key).unwrap_or_default()),
            fallback_source: if field(&layers.file.settings, key).is_some_and(|v| !v.is_null()) {
                "file"
            } else {
                "default"
            },
        }
    };
    let fields = EDITABLE.iter().map(|(k, live)| make(k, *live)).collect();
    let startup = STARTUP_ONLY.iter().map(|k| make(k, false)).collect();
    let restart_pending = layers.restart_pending();
    let can_undo = layers.store.can_undo();
    let list = layers.backend_list();
    let backends_locked = layers
        .backends_from_cli
        .then(|| "--backend-urls".to_string());
    drop(layers);

    let running = state.backends.lock_or_recover().clone();
    let configured: HashSet<String> = list.iter().map(|b| normalize_url(&b.url)).collect();
    let status = |url: &str| running.iter().find(|b| b.url == url);
    let mut backends: Vec<BackendCfgView> = list
        .iter()
        .map(|b| {
            let url = normalize_url(&b.url);
            let st = status(&url);
            let (slots, slots_detected) = st.map_or((0, false), |s| state.backend_capacity(s));
            BackendCfgView {
                label: backend_label(&url),
                name: b.name.clone(),
                token_set: b.token.as_deref().is_some_and(|t| !t.is_empty()),
                kind: b.kind.clone(),
                disabled: b.disabled,
                removed: false,
                online: st.is_some_and(|s| s.is_online),
                api_type: st.map_or("???", |s| s.api_type.display()),
                slots,
                slots_detected,
                url,
            }
        })
        .collect();
    for s in running.iter().filter(|s| !configured.contains(&s.url)) {
        backends.push(BackendCfgView {
            url: s.url.clone(),
            label: backend_label(&s.url),
            name: s.name.clone(),
            token_set: s.token.is_some(),
            kind: None,
            disabled: true,
            removed: true,
            online: false,
            api_type: s.api_type.display(),
            slots: 0,
            slots_detected: false,
        });
    }
    SettingsView {
        fields,
        startup,
        restart_pending,
        can_undo,
        backends,
        backends_locked,
    }
}

pub async fn get_settings(State(state): State<Arc<AppState>>) -> Json<SettingsView> {
    Json(view(&state))
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, msg)
}

/// Validate and save a new overlay, then apply it. The caller passes the
/// overlay to save; nothing changes when it is rejected.
fn commit(state: &Arc<AppState>, next: UiOverlay) -> Result<(), ApiError> {
    let (eff, list, fixed) = {
        let mut layers = state.config.lock_or_recover();
        let eff = layers
            .file
            .settings
            .overlay(&next.settings)
            .overlay(&layers.pinned);
        eff.validate().map_err(bad)?;
        if let Some(b) = &next.backends {
            crate::config::validate_backends(b).map_err(bad)?;
        }
        layers
            .store
            .save(next)
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        (eff, layers.backend_list(), layers.backends_from_cli)
    };
    settings::apply(state, &eff);
    if !fixed {
        settings::apply_backends(state, &list);
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct SettingsPatch {
    /// Setting name -> new value; `null` drops the UI's value, so the one
    /// from appconf.yaml (or the default) applies again.
    set: Map<String, Value>,
}

pub async fn put_settings(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<SettingsPatch>,
) -> ApiResult<SettingsView> {
    let mut next = state.config.lock_or_recover().store.overlay().clone();
    let mut ui = match serde_json::to_value(&next.settings) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    {
        let layers = state.config.lock_or_recover();
        for (key, value) in patch.set {
            if !EDITABLE.iter().any(|(k, _)| *k == key) {
                return Err(bad(format!("'{key}' cannot be changed here")));
            }
            if let Some(by) = layers.pinned_by.get(key.as_str()) {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    format!("'{key}' is set by {by}, which wins over the web UI"),
                ));
            }
            match value {
                Value::Null => {
                    ui.remove(&key);
                }
                Value::String(s) if SECRET.contains(&key.as_str()) && s == MASK => {}
                v => {
                    ui.insert(key, v);
                }
            }
        }
    }
    next.settings =
        serde_json::from_value::<Settings>(Value::Object(ui)).map_err(|e| bad(e.to_string()))?;
    commit(&state, next)?;
    Ok(Json(view(&state)))
}

pub async fn undo(State(state): State<Arc<AppState>>) -> ApiResult<SettingsView> {
    let (eff, list, fixed) = {
        let mut layers = state.config.lock_or_recover();
        layers.store.undo().map_err(bad)?;
        (
            layers.effective(),
            layers.backend_list(),
            layers.backends_from_cli,
        )
    };
    settings::apply(&state, &eff);
    if !fixed {
        settings::apply_backends(&state, &list);
    }
    Ok(Json(view(&state)))
}

/// The backend list as the UI edits it: the UI's copy, or the file's to start.
fn editable_backends(state: &AppState) -> Result<(UiOverlay, Vec<Backend>), ApiError> {
    let layers = state.config.lock_or_recover();
    if let Some(by) = layers.backends_from_cli.then_some("--backend-urls") {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("the backends are set by {by}, which wins over the web UI"),
        ));
    }
    Ok((layers.store.overlay().clone(), layers.backend_list()))
}

#[derive(Deserialize)]
pub struct BackendInput {
    url: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

fn clean(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub async fn add_backend(
    State(state): State<Arc<AppState>>,
    Json(input): Json<BackendInput>,
) -> ApiResult<SettingsView> {
    let (mut next, mut list) = editable_backends(&state)?;
    let url = normalize_url(&input.url);
    if list.iter().any(|b| normalize_url(&b.url) == url) {
        return Err(bad(format!("{url} is already a backend")));
    }
    list.push(Backend {
        url,
        name: clean(input.name),
        disabled: false,
        token: clean(input.token),
        kind: clean(input.kind),
    });
    next.backends = Some(list);
    commit(&state, next)?;
    Ok(Json(view(&state)))
}

#[derive(Deserialize)]
pub struct BackendUpdate {
    url: String,
    /// Absent: unchanged. `null` or empty: cleared.
    #[serde(default, with = "patch")]
    name: Option<Option<String>>,
    #[serde(default, with = "patch")]
    token: Option<Option<String>>,
    #[serde(default, with = "patch")]
    kind: Option<Option<String>>,
    #[serde(default)]
    disabled: Option<bool>,
}

/// Tells an absent field (`None`) from an explicit `null` (`Some(None)`).
mod patch {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<String>>, D::Error> {
        Option::<String>::deserialize(d).map(Some)
    }
}

pub async fn update_backend(
    State(state): State<Arc<AppState>>,
    Json(u): Json<BackendUpdate>,
) -> ApiResult<SettingsView> {
    let (mut next, mut list) = editable_backends(&state)?;
    let url = normalize_url(&u.url);
    let b = list
        .iter_mut()
        .find(|b| normalize_url(&b.url) == url)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("no backend {url}")))?;
    if let Some(v) = u.name {
        b.name = clean(v);
    }
    if let Some(v) = u.token {
        b.token = clean(v);
    }
    if let Some(v) = u.kind {
        b.kind = clean(v);
    }
    if let Some(v) = u.disabled {
        b.disabled = v;
    }
    next.backends = Some(list);
    commit(&state, next)?;
    Ok(Json(view(&state)))
}

#[derive(Deserialize)]
pub struct BackendRef {
    url: String,
}

/// Drop a backend from the list. It stops taking requests now and is gone
/// after the next restart.
pub async fn remove_backend(
    State(state): State<Arc<AppState>>,
    Json(r): Json<BackendRef>,
) -> ApiResult<SettingsView> {
    let (mut next, mut list) = editable_backends(&state)?;
    let url = normalize_url(&r.url);
    let before = list.len();
    list.retain(|b| normalize_url(&b.url) != url);
    if list.len() == before {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("no backend {url}"),
        ));
    }
    next.backends = Some(list);
    commit(&state, next)?;
    Ok(Json(view(&state)))
}

#[derive(Deserialize)]
pub struct TestInput {
    url: String,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Serialize)]
pub struct TestResult {
    online: bool,
    api_type: &'static str,
    slots: Option<u32>,
    model_count: usize,
    /// The first few models, sorted.
    models: Vec<String>,
}

/// Probe a URL the way the health loop does, without adding it.
pub async fn test_backend(
    State(state): State<Arc<AppState>>,
    Json(t): Json<TestInput>,
) -> Json<TestResult> {
    let url = normalize_url(&t.url);
    // No token typed: a configured backend is tested with its saved one.
    let token = clean(t.token).or_else(|| {
        state
            .backends
            .lock_or_recover()
            .iter()
            .find(|b| b.url == url)
            .and_then(|b| b.token.clone())
    });
    let probe = crate::control::probe_backend(
        &state.client,
        &url,
        token.as_deref(),
        &HashSet::new(),
        crate::control::PROBE_TIMEOUT,
    )
    .await;
    let mut models: Vec<String> = probe.available_models.into_iter().collect();
    models.sort();
    let model_count = models.len();
    models.truncate(12);
    Json(TestResult {
        online: probe.is_online,
        api_type: probe.api_type.display(),
        slots: probe.slots,
        model_count,
        models,
    })
}
