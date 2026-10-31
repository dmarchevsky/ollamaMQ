//! Web UI at `/ui`: the dashboard — live queue, backend and request state, plus
//! client and API-key management. Off unless a password is configured.
//!
//! Browsers sign in on `/ui/login` and get a session cookie (see
//! [`crate::session`]); scripts may instead send the same user and password
//! as HTTP basic auth. The page is static (embedded from `src/web/`) and
//! talks to the JSON API under `/ui/api/`. Every mutating call must carry
//! `X-OllamaMQ-UI: 1`: a custom header forces a CORS preflight, so another
//! site cannot ride the browser's credentials to change anything (the
//! cookie is also `SameSite=Strict`).

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{info, warn};

use crate::auth::{basic_credentials, constant_time_eq};
use crate::clients::{ClientPatch, StoreError};
use crate::control::{self, ControlAction, LoadOptions};
use crate::dispatcher::AppState;

mod overview;
mod settings;
use crate::lock::LockExt;
use crate::model_policy::{Action, Mode, Rule};
use crate::snapshot;

const INDEX_HTML: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const APP_CSS: &str = include_str!("web/app.css");
const DEMO_JS: &str = include_str!("web/demo.js");
const LOGIN_HTML: &str = include_str!("web/login.html");
const LOGIN_JS: &str = include_str!("web/login.js");
const FAVICON_SVG: &str = include_str!("web/favicon.svg");
const APPLE_TOUCH_ICON: &[u8] = include_bytes!("web/apple-touch-icon.png");

/// Reachable without signing in: the login page and what it needs.
const PUBLIC_PATHS: &[&str] = &[
    "/ui",
    "/ui/login",
    "/ui/login.js",
    "/ui/app.css",
    "/ui/favicon.svg",
    "/ui/apple-touch-icon.png",
    "/ui/api/login",
    "/ui/api/logout",
];

/// Upper bound on the conversations one listing call returns. The Logs
/// window itself no longer caps on this (log-viewer-share): a `since` call
/// returns everything the window holds, bounded only by retention.
const MAX_LOG_LIMIT: usize = 1000;
/// Hard safety bound on the rows `/api/logs` will assemble and serialise.
/// Not a window and not a count the operator is meant to reach — retention
/// is supposed to bite first; `truncated` reports honestly when this did.
const MAX_LOG_ROWS: usize = 200_000;

/// Header every mutating API call must carry (see the module docs).
const CSRF_HEADER: &str = "x-ollamamq-ui";

const CSP: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; \
                   img-src 'self' data:; frame-ancestors 'none'";

#[derive(Clone)]
enum Secret {
    /// From `OLLAMA_MQ_UI_PASSWORD` or `settings.web_ui.password`.
    Plain(String),
    /// argon2id hash from `settings.web_ui.password_hash` (written by the UI).
    Hash(String),
}

/// The web UI's credentials (one admin user).
#[derive(Clone)]
pub struct WebAuth {
    pub username: String,
    secret: Secret,
    /// Where the password comes from: `appconf.yaml` or `OLLAMA_MQ_UI_PASSWORD`.
    pub source: String,
    changed_at_ms: Option<u64>,
}

impl WebAuth {
    /// The strongest secret available: a hash (from `appconf.yaml`) wins,
    /// else the env or file plaintext password; `None` (web UI off) when
    /// neither exists. A malformed hash stops the proxy rather than
    /// silently falling back to the weaker password.
    pub fn resolve(
        username: Option<String>,
        hash: Option<(String, &str)>,
        password: Option<(String, &str)>,
    ) -> Option<Self> {
        let username = username.unwrap_or_else(|| "admin".to_string());
        if let Some((h, source)) = hash {
            if !crate::admin::valid_hash(&h) {
                eprintln!("Error: settings.web_ui.password_hash is not a valid password hash");
                std::process::exit(1);
            }
            return Some(Self {
                username,
                secret: Secret::Hash(h),
                source: source.to_string(),
                changed_at_ms: None,
            });
        }
        password.map(|(p, source)| Self {
            username,
            secret: Secret::Plain(p),
            source: source.to_string(),
            changed_at_ms: None,
        })
    }

    #[cfg(test)]
    pub fn plain(username: &str, password: &str) -> Self {
        Self {
            username: username.into(),
            secret: Secret::Plain(password.into()),
            source: "test".into(),
            changed_at_ms: None,
        }
    }

    fn check(&self, user: &str, pass: &str) -> bool {
        // Evaluate both so the response time does not reveal which one failed.
        let user_ok = constant_time_eq(user, &self.username);
        let pass_ok = match &self.secret {
            Secret::Plain(p) => constant_time_eq(pass, p),
            Secret::Hash(h) => crate::admin::verify(h, pass),
        };
        user_ok & pass_ok
    }
}

pub(crate) struct AuthState {
    /// Replaced when the password is changed in the UI.
    creds: std::sync::Mutex<WebAuth>,
    sessions: crate::session::Sessions,
    limiter: crate::session::LoginLimiter,
}

impl AuthState {
    /// Whether the request speaks for the operator: a live dashboard session
    /// cookie, or the UI's username and password as Basic auth (so `curl -u`
    /// uses the same credentials as the browser). The admin gate asks this.
    fn signed_in(&self, headers: &axum::http::HeaderMap) -> bool {
        let cookie = headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|c| crate::session::cookie_value(c, crate::session::COOKIE));
        if cookie.is_some_and(|t| self.sessions.user(t).is_some()) {
            return true;
        }
        let creds = self.creds.lock_or_recover().clone();
        basic_credentials(headers).is_some_and(|(u, p)| creds.check(&u, &p))
    }
}

/// The dashboard's credentials and its live sessions as one shared handle: the
/// router owns it, and the admin gate (see `admin.rs`) reads the same pair, so
/// "signed in" means the same thing on both sides — including after a password
/// change, which replaces the credentials inside this same handle.
pub fn auth_state(auth: WebAuth) -> Arc<AuthState> {
    Arc::new(AuthState {
        creds: std::sync::Mutex::new(auth),
        sessions: Default::default(),
        limiter: Default::default(),
    })
}

/// Whether `auth` lets this request act as the operator.
pub(crate) fn signed_in(auth: &AuthState, headers: &axum::http::HeaderMap) -> bool {
    auth.signed_in(headers)
}

/// Test hook: sign in as the dashboard would and hand back the cookie's token,
/// so a test outside this module can prove the session door works.
#[cfg(test)]
pub(crate) fn test_session(auth: &AuthState) -> String {
    let user = auth.creds.lock_or_recover().username.clone();
    auth.sessions.create(&user)
}

/// Routes for `/ui` and `/ui/api/*`; all but the login page need a session
/// (or basic-auth credentials).
/// Only the tests build a whole router from credentials; the binary goes
/// through `auth_state` so the admin gate can share the handle.
#[cfg(test)]
pub fn router(auth: WebAuth) -> Router<Arc<AppState>> {
    router_with_state(auth_state(auth))
}

pub fn router_with_state(auth: Arc<AuthState>) -> Router<Arc<AppState>> {
    let api = Router::new()
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/me", get(me))
        .route("/state", get(get_state))
        .route("/overview", get(overview::get_overview))
        .route("/logs", get(list_logs))
        .route("/logs/{seq}", get(get_log))
        .route("/capture", get(capture_state))
        .route("/audit", get(audit_status))
        .route("/convs", get(list_convs))
        .route("/conv", get(get_conv))
        .route("/conv/content", get(get_conv_content))
        .route("/users/{id}/{op}", post(user_action))
        .route("/sessions/{tag}/priority", post(session_priority))
        .route("/blocked/unblock", post(unblock))
        .route("/backends/{idx}/{op}", post(model_control))
        .route("/config/reload", post(reload_config))
        .route("/account/password", post(change_password))
        .route("/settings", get(settings::get_settings).put(settings::put_settings))
        .route("/settings/undo", post(settings::undo))
        .route("/settings/backends", post(settings::add_backend).put(settings::update_backend))
        .route("/settings/backends/remove", post(settings::remove_backend))
        .route("/settings/backends/test", post(settings::test_backend))
        .route("/clients", get(list_clients).post(create_client))
        .route(
            "/clients/{name}",
            axum::routing::patch(update_client).delete(delete_client),
        )
        .route("/clients/{name}/keys", post(add_key))
        .route("/models", get(get_models))
        .route("/models/mode", axum::routing::put(set_model_mode))
        .route("/models/rules", post(add_model_rule))
        .route(
            "/models/rules/{id}",
            axum::routing::delete(delete_model_rule),
        )
        .route("/models/toggle", post(toggle_model))
        .route(
            "/keys/{id}",
            axum::routing::patch(patch_key).delete(delete_key),
        )
        .fallback(|| async { ApiError::new(StatusCode::NOT_FOUND, "no such API endpoint") })
        .layer(middleware::from_fn(csrf_guard));

    Router::new()
        .route("/ui", get(|| async { Redirect::permanent("/ui/") }))
        .route(
            "/ui/",
            get(|| async { asset(INDEX_HTML, "text/html; charset=utf-8") }),
        )
        .route(
            "/ui/app.js",
            get(|| async { asset(APP_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/ui/app.css",
            get(|| async { asset(APP_CSS, "text/css; charset=utf-8") }),
        )
        .route(
            "/ui/demo.js",
            get(|| async { asset(DEMO_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/ui/login",
            get(|| async { asset(LOGIN_HTML, "text/html; charset=utf-8") }),
        )
        .route(
            "/ui/favicon.svg",
            get(|| async { icon(FAVICON_SVG.as_bytes(), "image/svg+xml") }),
        )
        .route(
            "/ui/apple-touch-icon.png",
            get(|| async { icon(APPLE_TOUCH_ICON, "image/png") }),
        )
        .route(
            "/ui/login.js",
            get(|| async { asset(LOGIN_JS, "text/javascript; charset=utf-8") }),
        )
        .nest("/ui/api", api)
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(auth.clone(), require_auth))
        .layer(axum::Extension(auth))
        .layer(middleware::from_fn(security_headers))
}

/// Icons rarely change: let browsers keep them for a day.
fn icon(body: &'static [u8], content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        body,
    )
        .into_response()
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

// ------------------------------------------------------------------ guards

/// The signed-in user: a live session cookie, or basic-auth credentials
/// (for scripts).
fn authenticated(auth: &AuthState, headers: &axum::http::HeaderMap) -> Option<String> {
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| crate::session::cookie_value(c, crate::session::COOKIE));
    if let Some(user) = cookie.and_then(|t| auth.sessions.user(t)) {
        return Some(user);
    }
    let creds = auth.creds.lock_or_recover().clone();
    basic_credentials(headers)
        .filter(|(u, p)| creds.check(u, p))
        .map(|(u, _)| u)
}

async fn require_auth(
    State(auth): State<Arc<AuthState>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if PUBLIC_PATHS.contains(&path) || authenticated(&auth, request.headers()).is_some() {
        return next.run(request).await;
    }
    // API callers get a plain 401 (no `WWW-Authenticate`, so browsers never
    // pop their own credentials dialog); pages go to the sign-in form and
    // come back afterwards.
    if path.starts_with("/ui/api/") {
        return ApiError::new(StatusCode::UNAUTHORIZED, "sign in required").into_response();
    }
    let back: String = url_escape(path);
    Redirect::to(&format!("/ui/login?next={back}")).into_response()
}

/// Percent-encode a path for a query value (what `encodeURIComponent` does).
fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut res = next.run(request).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    res
}

async fn csrf_guard(request: Request, next: Next) -> Response {
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    if !safe && request.headers().get(CSRF_HEADER).is_none_or(|v| v != "1") {
        return ApiError::new(StatusCode::FORBIDDEN, "missing X-OllamaMQ-UI header")
            .into_response();
    }
    let mut res = next.run(request).await;
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

// ------------------------------------------------------------------ errors

struct ApiError(StatusCode, String);

impl ApiError {
    fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        Self(status, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        let status = match e {
            StoreError::Invalid(_) => StatusCode::BAD_REQUEST,
            StoreError::NotFound(_) => StatusCode::NOT_FOUND,
            StoreError::Conflict(_) => StatusCode::CONFLICT,
            StoreError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, e.to_string())
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

fn ok() -> ApiResult<serde_json::Value> {
    Ok(Json(json!({})))
}

// ------------------------------------------------------------------- state

#[derive(Serialize)]
struct StateView {
    version: &'static str,
    stats: Stats,
    vip: Option<String>,
    boost: Option<String>,
    backends: Vec<BackendView>,
    users: Vec<UserView>,
    blocked: BlockedView,
    /// Requests being served right now, oldest first.
    runs: Vec<RunView>,
}

#[derive(Serialize)]
struct RunView {
    client: String,
    /// API key label, or `none`.
    key: String,
    model: String,
    backend: String,
    /// `prefill` until the first token arrives, then `generating`.
    state: &'static str,
    /// Time it waited in the queue before dispatch.
    waited_secs: f64,
    elapsed_secs: f64,
    /// Time to first byte; `None` while the backend has not answered yet.
    ttfb_secs: Option<f64>,
    /// Time to the first generated token; `None` while prefilling (or for a
    /// response that is not streamed).
    ttft_secs: Option<f64>,
    /// Generated tokens so far (estimated from the stream).
    tokens: u64,
    /// Generation speed since the first token.
    tokens_per_sec: Option<f64>,
    /// Prompt tokens estimated from the request body (bytes ÷ 4) at dispatch.
    est_prompt_tokens: u64,
    /// Conversation tag for the session views; `None` when there is none.
    session: Option<String>,
    /// Human label the client sent for that conversation; display only.
    session_label: Option<String>,
    /// Scheduling level this request is being served at.
    priority: &'static str,
    /// Log correlation id, so the row can jump to the request's log round.
    rid: u64,
    /// Absolute dispatch time (unix ms), measured at dispatch.
    dispatch_ms: u64,
    /// Absolute first-token time (unix ms); `None` while prefilling.
    first_token_ms: Option<u64>,
}

#[derive(Serialize)]
struct Stats {
    queued: usize,
    processing: usize,
    done: usize,
    dropped: usize,
    log_dropped: u64,
}

#[derive(Serialize)]
struct BackendView {
    idx: usize,
    url: String,
    /// Display name; the UI shows `host:port` when unset.
    name: Option<String>,
    /// Turned off in the settings (never probed or routed to).
    disabled: bool,
    online: bool,
    api_type: &'static str,
    active: usize,
    max: u32,
    /// `max` is the slot count the backend reports (else the configured cap).
    max_detected: bool,
    processed: usize,
    current_model: Option<String>,
    models_available: Vec<String>,
    models_loaded: Vec<String>,
    /// Listed models the model policy keeps this backend from serving.
    models_hidden: Vec<String>,
    /// False when the backend cannot report what is resident (then every
    /// available model counts as ready).
    loaded_known: bool,
    loaded_ctx: BTreeMap<String, u64>,
    can_control: bool,
    active_op: Option<OpView>,
    results: Vec<ResultView>,
}

#[derive(Serialize)]
struct OpView {
    verb: String,
    model: String,
    elapsed_secs: u64,
}

#[derive(Serialize)]
struct ResultView {
    ok: bool,
    verb: String,
    model: String,
    error: Option<String>,
}

#[derive(Serialize)]
struct UserView {
    id: String,
    ip: Option<String>,
    queued: usize,
    processing: usize,
    done: usize,
    dropped: usize,
    /// The id is a registered client's name (only its keys can use it).
    registered: bool,
    blocked: bool,
}

#[derive(Serialize)]
struct BlockedView {
    /// Blocked ids that are not registered clients.
    users: Vec<String>,
    ips: Vec<String>,
    /// Disabled registered clients (blocking a client disables it).
    clients: Vec<String>,
}

fn sorted(set: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.iter().cloned().collect();
    v.sort();
    v
}

async fn get_state(State(state): State<Arc<AppState>>) -> Json<StateView> {
    let snap = snapshot::capture(&state, 0);
    let policy = state.model_policy();

    let backends = snap
        .backends
        .iter()
        .enumerate()
        .map(|(idx, b)| BackendView {
            idx,
            url: b.url.clone(),
            name: b.name.clone(),
            disabled: b.disabled,
            online: b.is_online,
            api_type: b.api_type.display(),
            active: b.active_requests,
            max: state.backend_capacity(b).0,
            max_detected: state.backend_capacity(b).1,
            processed: b.processed_count,
            current_model: b.current_model.clone(),
            models_available: sorted(&b.available_models),
            models_loaded: sorted(&b.loaded_models),
            models_hidden: {
                let mut v: Vec<String> = b
                    .available_models
                    .iter()
                    .filter(|m| !policy.served(m, &b.url))
                    .cloned()
                    .collect();
                v.sort();
                v
            },
            loaded_known: b.loaded_state_known,
            loaded_ctx: b.loaded_ctx.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            can_control: control::supports_control(b),
            active_op: snap
                .active_ops
                .iter()
                .find(|op| op.backend_idx == idx)
                .map(|op| OpView {
                    verb: op.verb.clone(),
                    model: op.model.clone(),
                    elapsed_secs: op.elapsed_secs,
                }),
            results: snap
                .recent_results
                .iter()
                .filter(|r| r.backend_idx == idx)
                .map(|r| ResultView {
                    ok: r.ok,
                    verb: r.verb.clone(),
                    model: r.model.clone(),
                    error: r.error.clone(),
                })
                .collect(),
        })
        .collect();

    let users = snap
        .users
        .iter()
        .map(|u| UserView {
            id: u.id.clone(),
            ip: u.ip.map(|ip| ip.to_string()),
            queued: u.queued,
            processing: u.processing,
            done: u.processed,
            dropped: u.dropped,
            registered: u.registered,
            blocked: snap.blocked_users.contains(&u.id)
                || u.ip.is_some_and(|ip| snap.blocked_ips.contains(&ip)),
        })
        .collect();

    let mut ips: Vec<String> = snap.blocked_ips.iter().map(|ip| ip.to_string()).collect();
    ips.sort();

    Json(StateView {
        version: env!("CARGO_PKG_VERSION"),
        stats: Stats {
            queued: snap.users.iter().map(|u| u.queued).sum(),
            processing: snap.users.iter().map(|u| u.processing).sum(),
            done: snap.users.iter().map(|u| u.processed).sum(),
            dropped: snap.users.iter().map(|u| u.dropped).sum(),
            log_dropped: snap.reqlog_dropped,
        },
        vip: snap.vip_user,
        boost: snap.boost_user,
        backends,
        users,
        runs: state
            .runs
            .list()
            .into_iter()
            .map(|r| {
                let ttfb = r.first_byte.map(|f| (f - r.started).as_secs_f64());
                let ttft = r.first_token.map(|f| (f - r.started).as_secs_f64());
                let gen_secs = r.first_token.map(|f| f.elapsed().as_secs_f64());
                RunView {
                    state: if r.first_token.is_some() { "generating" } else { "prefill" },
                    waited_secs: r.waited_secs,
                    elapsed_secs: r.started.elapsed().as_secs_f64(),
                    ttfb_secs: ttfb,
                    ttft_secs: ttft,
                    tokens_per_sec: gen_secs
                        .filter(|s| *s >= 1.0 && r.tokens > 0)
                        .map(|s| r.tokens as f64 / s),
                    tokens: r.tokens,
                    est_prompt_tokens: r.est_prompt_tokens,
                    session: r.session,
                    session_label: r.session_label,
                    priority: r.priority,
                    rid: r.rid,
                    dispatch_ms: r.dispatch_ms,
                    first_token_ms: r.first_token_ms,
                    client: r.client,
                    key: r.key,
                    model: r.model,
                    backend: r.backend,
                }
            })
            .collect(),
        blocked: BlockedView {
            users: sorted(
                &snap
                    .blocked_users
                    .difference(&snap.disabled_clients)
                    .cloned()
                    .collect(),
            ),
            ips,
            clients: sorted(&snap.disabled_clients),
        },
    })
}

// -------------------------------------------------------------------- logs

#[derive(Serialize)]
struct LogView {
    seq: u64,
    at_ms: u64,
    dir: &'static str,
    user: String,
    model: Option<String>,
    backend: Option<String>,
    info: String,
    has_content: bool,
    /// True when the body text was left out on purpose (capture is off for this
    /// client, key or backend) rather than because there was nothing to store.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    redacted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    /// Correlation id of the request this event belongs to; IN and OUT of one
    /// request share it — the UI pairs log lines with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    rid: Option<u64>,
    /// Conversation tag of the request; lets the UI filter a conversation's
    /// rounds and find the previous round for the "new only" view.
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    /// The label the client sent for that session, when the conversation log
    /// knows it. The grouped Logs list reads `#cache-warmup` instead of the
    /// six-hex tag, which is what makes one row per conversation recognisable.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_label: Option<String>,
    /// True on rows synthesized from the Activity journal rather than the
    /// live ring: older than anything the ring held, restored at boot. The UI
    /// opens such a row through the conversation log, never `logs/{seq}`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    hist: bool,
    /// Security-audit flags this request earned, worst first (empty = clean).
    /// They land seconds after the request finished, so a row can gain them
    /// while the page sits open — that is the point of the audit, not a bug.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    flags: Vec<crate::audit::Flag>,
}

fn ring_at_ms(ev: &crate::dispatcher::LogEvent) -> u64 {
    ev.at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn log_view(
    seq: u64,
    ev: &crate::dispatcher::LogEvent,
    with_content: bool,
    session_label: Option<String>,
    flags: Vec<crate::audit::Flag>,
) -> LogView {
    LogView {
        seq,
        at_ms: ring_at_ms(ev),
        dir: ev.dir,
        user: ev.user.clone(),
        model: ev.model.clone(),
        backend: ev.backend.clone(),
        info: ev.info.clone(),
        has_content: ev.content.as_deref().is_some_and(|c| !c.is_empty()),
        redacted: ev.redacted,
        rid: ev.rid,
        session: ev.session.clone(),
        session_label,
        hist: false,
        flags,
        content: if with_content {
            ev.content.as_deref().map(|c| c.to_string())
        } else {
            None
        },
    }
}

/// Session tag -> the label its client sent, from what the log has recorded.
/// Nothing here reads the ring: an uncaptured or label-less session simply has
/// no entry, and the UI shows the tag.
fn session_labels(state: &AppState) -> HashMap<String, String> {
    state
        .convlog
        .summaries(MAX_LOG_LIMIT)
        .into_iter()
        .filter_map(|c| Some((c.session?, c.session_label?)))
        .collect()
}

#[derive(Deserialize)]
struct LogQuery {
    limit: Option<usize>,
    /// Window start (unix ms). With `since` the answer is the whole window —
    /// every event at or after it, live ring and journalled history alike —
    /// and `limit` is ignored. This is the shape the Logs page polls with;
    /// the old count shape (`limit` alone) keeps working for old callers and
    /// defaults to 300 events when neither parameter is given.
    since: Option<u64>,
    /// How wide the caller's window is (ms), sent alongside `since`. `since`
    /// slides forward with every poll, so it cannot identify the ANSWER — two
    /// polls one second apart ask the same question in every way that matters.
    /// The change-tag hashes this instead, which is what makes the 304 path
    /// reachable from a live 1 s poll at all.
    win: Option<u64>,
}

/// One `/api/logs` answer: the window itself, plus an honest account of its
/// reach. The list used to be a bare array capped at 300 events, which one
/// chatty conversation could collapse to itself; now the window is the
/// contract, retention is the only limit, and a row that cannot open says
/// so before it is ever clicked.
#[derive(Serialize)]
struct LogsPage {
    events: Vec<LogView>,
    /// Oldest `at_ms` in `events` (null for an empty window).
    oldest_ms: Option<u64>,
    /// Oldest moment the conversation log can still resolve. Rows older than
    /// this render as inert history — clicking them can open nothing. Null
    /// when unknown (capture off, or nothing indexed yet).
    chain_start_ms: Option<u64>,
    /// True only when the hard safety bound (MAX_LOG_ROWS) refused to serve
    /// the whole window. It has no count-cap meaning any more.
    truncated: bool,
    /// The boot scan of the rotation chain is still filling the disk index:
    /// the retained history is arriving, the list is not settled yet.
    indexing: bool,
}

/// One ring event as a `LogView`, with its session label and audit flags.
fn ring_log_view(
    state: &AppState,
    labels: &HashMap<String, String>,
    seq: u64,
    ev: &crate::dispatcher::LogEvent,
) -> LogView {
    let label = ev.session.as_ref().and_then(|s| labels.get(s).cloned());
    let flags = ev
        .rid
        .and_then(|rid| state.audit.flags_for(rid))
        .unwrap_or_default();
    log_view(seq, ev, false, label, flags)
}

/// Hash of the numbers that change exactly when a Logs window changes:
/// newest seq (ring growth and eviction alike), rows in the window on both
/// sides, the journal's newest stamp, the chain reach, the indexing bit and
/// the live audit-verdict count (so a flag landing after its row was served
/// still changes the tag). Cheap by construction — this is what a 1 s poll
/// answers a 304 from.
fn logs_tag(parts: [u64; 9]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
    }
    format!("\"{:016x}\"", h.finish())
}

/// Newest first, without content (fetch one event for that). The live ring
/// comes first; under it the Activity journal fills everything the ring has
/// forgotten — including everything from before this process started, so the
/// panel opens with its history instead of a blank page. Replayed records the
/// ring still holds are the same requests (deduped by rid); rids are unique
/// across restarts because the counter is seeded from the retained files.
/// With `since` the whole window comes back (no count cap below the safety
/// bound), and an `If-None-Match` carrying the previous change-tag answers a
/// 304 with no body, so the 1 s poll costs nothing while nothing is new.
async fn list_logs(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<LogQuery>,
) -> Response {
    let since = q.since;
    let limit = q.limit.unwrap_or(300).min(MAX_LOG_ROWS);
    let chain_start_ms = state.convlog.chain_start_ms();
    let indexing = state.convlog.indexing();
    // The cheap change-tag first, without building a single view.
    let (hist_count, hist_newest) = state.stats.window_tag(since.unwrap_or(0));
    let ring_tag = {
        let logs = state.logs.lock_or_recover();
        let base = state.logs_evicted.load(Ordering::Relaxed);
        let in_window = match since {
            Some(ms) => logs.iter().filter(|e| ring_at_ms(e) >= ms).count(),
            None => logs.len().min(limit),
        };
        (base + logs.len() as u64, in_window as u64)
    };
    let tag = logs_tag([
        // The window's IDENTITY, not its moving start: asked one second later
        // the question is the same question, so hashing `since` alone would
        // make a 304 impossible on a live poll. Callers that send no `win`
        // (old count-shaped ones) keep the previous behaviour.
        match (q.win, since) {
            (Some(w), _) => w,
            (None, Some(s)) => s,
            (None, None) => u64::MAX,
        },
        limit as u64,
        ring_tag.0,
        ring_tag.1,
        hist_count as u64,
        hist_newest,
        chain_start_ms.unwrap_or(0),
        indexing as u64,
        state.audit.live_flags(),
    ]);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().trim_start_matches("W/") == tag)
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, tag)]).into_response();
    }
    let labels = session_labels(&state);
    let (mut views, oldest_ring, seen_rids, next_seq) = {
        let logs = state.logs.lock_or_recover();
        let base = state.logs_evicted.load(Ordering::Relaxed);
        let views: Vec<LogView> = match since {
            // The whole window: every ring event at or after `since`. The
            // ring is a memory bound only — its size has never been a window.
            Some(ms) => logs
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, ev)| ring_at_ms(ev) >= ms)
                .map(|(i, ev)| ring_log_view(&state, &labels, base + i as u64, ev))
                .collect(),
            None => logs
                .iter()
                .enumerate()
                .rev()
                .take(limit)
                .map(|(i, ev)| ring_log_view(&state, &labels, base + i as u64, ev))
                .collect(),
        };
        (
            views,
            logs.front().map(ring_at_ms).unwrap_or(u64::MAX),
            logs.iter()
                .filter_map(|e| e.rid)
                .collect::<std::collections::HashSet<u64>>(),
            base + logs.len() as u64,
        )
    };
    // The journal's half of the window, newest first under the ring. With
    // `since` this is the whole walk — no `finished_tail` slicing, no event
    // count, the MAX_LOG_ROWS safety bound may still bite and then says so.
    let mut replay: Vec<crate::stats::Finished> = match since {
        Some(ms) => {
            let mut all: Vec<_> = state
                .stats
                .finished_since(ms)
                .into_iter()
                .filter(|f| f.at_ms < oldest_ring && !f.rid.is_some_and(|r| seen_rids.contains(&r)))
                .collect();
            all.reverse(); // the walk is oldest first; the list reads newest first
            all
        }
        None if views.len() < limit => {
            let need = limit - views.len();
            // Margin: ring-resident replays are filtered out below, and there
            // can be at most one per rid the ring holds.
            state
                .stats
                .finished_tail(need + seen_rids.len())
                .into_iter()
                .filter(|f| f.at_ms < oldest_ring && !f.rid.is_some_and(|r| seen_rids.contains(&r)))
                .take(need)
                .collect()
        }
        None => Vec::new(),
    };
    let truncated = replay.len() > MAX_LOG_ROWS;
    replay.truncate(MAX_LOG_ROWS);
    // Which rows can open: ONE bulk set of the rids the conversation log can
    // resolve. A journalled row whose rid is not in it names a conversation
    // that rotated out of the chain — it renders as inert history, never as
    // a click that 404s.
    let openable = if replay.is_empty() {
        HashSet::new()
    } else {
        state.convlog.resolvable_rids()
    };
    views.extend(replay.into_iter().enumerate().map(|(i, f)| {
        let label = f
            .session_label
            .clone()
            .or_else(|| f.session.as_ref().and_then(|s| labels.get(s).cloned()));
        LogView {
            seq: next_seq + i as u64,
            at_ms: f.at_ms,
            dir: "OUT",
            user: f.client,
            model: if f.model == "none" { None } else { Some(f.model) },
            backend: f.backend,
            info: f.outcome.to_string(),
            // The row opens through the conversation log, so it has content
            // exactly when the log can resolve its rid.
            has_content: f.rid.is_some_and(|r| openable.contains(&r)),
            redacted: false,
            content: None,
            rid: f.rid,
            session: f.session,
            session_label: label,
            hist: true,
            flags: f
                .rid
                .and_then(|rid| state.audit.flags_for(rid))
                .unwrap_or_default(),
        }
    }));
    let oldest_ms = views.iter().map(|v| v.at_ms).min();
    let mut resp = Json(LogsPage {
        events: views,
        oldest_ms,
        chain_start_ms,
        truncated,
        indexing,
    })
    .into_response();
    if let Ok(v) = header::HeaderValue::from_str(&tag) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    resp
}

async fn get_log(State(state): State<Arc<AppState>>, Path(seq): Path<u64>) -> ApiResult<LogView> {
    let labels = session_labels(&state);
    let logs = state.logs.lock_or_recover();
    let base = state.logs_evicted.load(Ordering::Relaxed);
    seq.checked_sub(base)
        .and_then(|i| logs.get(i as usize))
        .map(|ev| {
            let label = ev
                .session
                .as_ref()
                .and_then(|s| labels.get(s).cloned());
            let flags = ev
                .rid
                .and_then(|rid| state.audit.flags_for(rid))
                .unwrap_or_default();
            Json(log_view(seq, ev, true, label, flags))
        })
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "log event has expired"))
}

// --------------------------------------------------------- conversations
//
// The log viewer's data: one conversation (all of its turns, with real
// timestamps) rather than one request round. The index carries no body text —
// a conversation resends its whole history every turn, so sending the text with
// the index would refetch hundreds of kilobytes on every navigation step; text
// is fetched per turn on demand.

#[derive(Deserialize)]
struct ConvQuery {
    limit: Option<usize>,
    /// Conversation key verbatim (`client#session`).
    key: Option<String>,
    session: Option<String>,
    user: Option<String>,
    rid: Option<u64>,
    /// `in` (request) or `out` (response); anything else means the request.
    dir: Option<String>,
}

fn find_of(q: &ConvQuery) -> crate::convlog::Find<'_> {
    crate::convlog::Find {
        key: q.key.as_deref(),
        session: q.session.as_deref(),
        user: q.user.as_deref(),
        rid: q.rid,
    }
}

async fn list_convs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ConvQuery>,
) -> Json<Vec<crate::convlog::ConvSummary>> {
    Json(state.convlog.summaries(q.limit.unwrap_or(200).min(MAX_LOG_LIMIT)))
}

/// Whether body capture is actually running, and where it writes. The log path
/// is opened at boot, so a path that cannot be opened (a missing `/data` on a
/// host that expects it) leaves the whole capture silently off — the Logs page
/// needs to say that rather than fail every row open with the same error an
/// evicted conversation produces.
async fn capture_state(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "log": state.convlog.enabled(),
        "path": state.convlog.path(),
        // The boot scan may still be filling the disk index, and the chain
        // may reach back less far than the window the Logs page offers: the
        // UI says "indexing the log…" or names retention rather than
        // letting rows open with a 404.
        "indexing": state.convlog.indexing(),
        "chain_back_ms": state.convlog.chain_start_ms(),
    }))
}

/// How the security audit stands: mode, backend and its state, queue and drops.
/// The Settings page shows one quiet line built from this; the numbers are the
/// ones the operator needs to see the audit fall behind or lose its backend.
async fn audit_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(state.audit.status(&state))
}

async fn get_conv(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ConvQuery>,
) -> ApiResult<crate::convlog::Conv> {
    let missing = if !state.convlog.enabled() {
        "body capture is off — set a writable Request log path in Settings and restart to store conversations"
    } else if state.convlog.indexing() {
        "the conversation log is still indexing the retained files — try again in a moment"
    } else {
        "no such conversation: it aged out of the log chain — raise request_log_max_files \
         (and request_log_max_bytes) to keep more slots"
    };
    state
        .convlog
        .index(&find_of(&q))
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, missing))
}

async fn get_conv_content(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ConvQuery>,
) -> ApiResult<serde_json::Value> {
    let dir = match q.dir.as_deref() {
        Some("out") => crate::convlog::Dir::Resp,
        _ => crate::convlog::Dir::Req,
    };
    let content = state
        .convlog
        .content(&find_of(&q), dir)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no captured text for this request"))?;
    Ok(Json(json!({ "content": content })))
}

// ------------------------------------------------------- users & blocking

/// Operator pin of a session's priority, by the six-hex tag the UI shows:
/// `{"level": "normal" | "high" | "top"}`. A pin outranks whatever the client
/// claims, including pinning a session down at `normal`. See
/// [`AppState::pin_session`].
async fn session_priority(
    State(state): State<Arc<AppState>>,
    Path(tag): Path<String>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> ApiResult<serde_json::Value> {
    let level = body
        .get("level")
        .and_then(|v| v.as_str())
        .and_then(crate::dispatcher::Priority::from_name)
        .ok_or_else(|| {
            ApiError::new(StatusCode::BAD_REQUEST, "level must be normal, high or top")
        })?;
    // A pin is keyed by the tag, so it can always be set: the conversation may
    // be mid-first-request, where nothing about it is resolvable yet.
    if tag.len() != 6 || !tag.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "session tag must be six hex digits",
        ));
    }
    state.pin_session(&tag, level);
    ok()
}

async fn user_action(
    State(state): State<Arc<AppState>>,
    Path((id, op)): Path<(String, String)>,
) -> ApiResult<serde_json::Value> {
    let ip = state.user_ips.lock_or_recover().get(&id).copied();
    match op.as_str() {
        "vip" => state.toggle_vip(&id),
        "boost" => state.toggle_boost(&id),
        "block" => state.block_user(id),
        "block-ip" => {
            let ip = ip.ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "no IP address recorded for this user",
                )
            })?;
            state.block_ip(ip);
        }
        // Unblock on a user row: lift both blocks.
        "unblock" => {
            state.unblock_user(&id);
            if let Some(ip) = ip {
                state.unblock_ip(ip);
            }
        }
        _ => return Err(ApiError::new(StatusCode::NOT_FOUND, "unknown user action")),
    }
    ok()
}

#[derive(Deserialize)]
struct UnblockBody {
    kind: String,
    value: String,
}

async fn unblock(
    State(state): State<Arc<AppState>>,
    Json(body): Json<UnblockBody>,
) -> ApiResult<serde_json::Value> {
    match body.kind.as_str() {
        // A client's block is its disabled flag; `unblock_user` lifts both.
        "user" | "client" => state.unblock_user(&body.value),
        "ip" => {
            let ip = body
                .value
                .parse()
                .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid IP address"))?;
            state.unblock_ip(ip);
        }
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "kind must be 'user', 'client' or 'ip'",
            ));
        }
    }
    ok()
}

// ------------------------------------------------------- backends & config

#[derive(Deserialize)]
struct ModelBody {
    model: String,
}

async fn model_control(
    State(state): State<Arc<AppState>>,
    Path((idx, op)): Path<(usize, String)>,
    Json(body): Json<ModelBody>,
) -> ApiResult<serde_json::Value> {
    let action = match op.as_str() {
        "load" => ControlAction::Load,
        "unload" => ControlAction::Unload,
        _ => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown backend action",
            ));
        }
    };
    control::start_model_control(&state, idx, action, body.model, LoadOptions::default())
        .map(|model| Json(json!({ "model": model })))
        .map_err(|e| ApiError::new(StatusCode::CONFLICT, e))
}

async fn reload_config(State(state): State<Arc<AppState>>) -> ApiResult<serde_json::Value> {
    crate::settings::reload(&state)
        .map(|n| Json(json!({ "models": n })))
        .map_err(|e| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("config reload failed: {e}"),
            )
        })
}

// ------------------------------------------------------- clients & keys

async fn list_clients(State(state): State<Arc<AppState>>) -> Json<Vec<crate::clients::ClientView>> {
    Json(state.clients.lock_or_recover().list())
}

#[derive(Deserialize)]
struct NewClient {
    name: String,
    #[serde(default)]
    note: String,
}

async fn create_client(
    State(state): State<Arc<AppState>>,
    Json(body): Json<NewClient>,
) -> ApiResult<serde_json::Value> {
    state
        .clients
        .lock_or_recover()
        .create_client(&body.name, &body.note)?;
    state.client_registered(body.name.trim());
    info!("web: client '{}' created", body.name.trim());
    ok()
}

async fn update_client(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(patch): Json<ClientPatch>,
) -> ApiResult<serde_json::Value> {
    let disabled = patch.disabled;
    let renamed = patch
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| *n != name)
        .map(str::to_string);
    state
        .clients
        .lock_or_recover()
        .update_client(&name, patch)?;
    if let Some(new) = &renamed {
        state.rename_identity(&name, new);
        info!("web: client '{}' renamed to '{}'", name, new);
    }
    match disabled {
        Some(true) => warn!("web: client '{}' disabled", name),
        Some(false) => info!("web: client '{}' enabled", name),
        None => info!("web: client '{}' updated", name),
    }
    ok()
}

async fn delete_client(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<serde_json::Value> {
    state.clients.lock_or_recover().delete_client(&name)?;
    state.forget_client(&name);
    warn!("web: client '{}' deleted", name);
    ok()
}

#[derive(Deserialize)]
struct NewKey {
    /// Absent = generate one.
    key: Option<String>,
    #[serde(default)]
    label: String,
    #[serde(default)]
    redact_log: bool,
}

async fn add_key(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<NewKey>,
) -> ApiResult<crate::clients::ApiKey> {
    let key = state
        .clients
        .lock_or_recover()
        .add_key(&name, body.key.as_deref(), &body.label, body.redact_log)?;
    // The key itself is never logged.
    info!("web: key {} added to client '{}'", key.id, name);
    Ok(Json(key))
}

#[derive(Deserialize)]
struct KeyPatch {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    redact_log: Option<bool>,
}

/// `PATCH /keys/{id}` — rename its label and/or switch its body-text capture,
/// or both; at least one field must be present. A key never changes owner: that
/// is a delete plus a new key.
async fn patch_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<KeyPatch>,
) -> ApiResult<serde_json::Value> {
    if body.label.is_none() && body.redact_log.is_none() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "nothing to change: pass 'label' and/or 'redact_log'",
        ));
    }
    state
        .clients
        .lock_or_recover()
        .update_key(&id, body.label.as_deref(), body.redact_log)?;
    info!("web: key {id} updated (label and/or capture switch)");
    ok()
}

async fn delete_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    state.clients.lock_or_recover().delete_key(&id)?;
    warn!("web: key {} revoked", id);
    ok()
}

// ------------------------------------------------------------- sign-in

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

fn session_cookie(token: &str, max_age: u64, secure: bool) -> String {
    format!(
        "{}={token}; Path=/ui; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
        crate::session::COOKIE,
        if secure { "; Secure" } else { "" }
    )
}

/// Served over HTTPS (directly or behind a TLS-terminating proxy): only
/// then may the cookie be marked `Secure`.
fn is_https(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"))
}

async fn login(
    axum::Extension(auth): axum::Extension<Arc<AuthState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    if let Some(secs) = auth.limiter.locked_for() {
        warn!("web: sign-in refused, too many failed attempts");
        return ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many failed sign-ins; try again in {secs}s"),
        )
        .into_response();
    }
    let creds = auth.creds.lock_or_recover().clone();
    if !creds.check(&body.username, &body.password) {
        auth.limiter.fail();
        warn!("web: failed sign-in for user '{}'", body.username);
        return ApiError::new(StatusCode::UNAUTHORIZED, "wrong user name or password")
            .into_response();
    }
    auth.limiter.succeed();
    let token = auth.sessions.create(&body.username);
    info!("web: '{}' signed in", body.username);
    let cookie = session_cookie(
        &token,
        crate::session::SESSION_TTL.as_secs(),
        is_https(&headers),
    );
    (
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "user": body.username })),
    )
        .into_response()
}

async fn logout(
    axum::Extension(auth): axum::Extension<Arc<AuthState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Some(token) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| crate::session::cookie_value(c, crate::session::COOKIE))
    {
        auth.sessions.remove(token);
    }
    (
        [(
            header::SET_COOKIE,
            session_cookie("", 0, is_https(&headers)),
        )],
        Json(json!({})),
    )
        .into_response()
}

async fn me(
    axum::Extension(auth): axum::Extension<Arc<AuthState>>,
    headers: axum::http::HeaderMap,
) -> Json<serde_json::Value> {
    let creds = auth.creds.lock_or_recover().clone();
    Json(json!({
        "user": authenticated(&auth, &headers),
        "password_source": creds.source,
        "password_changed_at_ms": creds.changed_at_ms,
    }))
}

#[derive(Deserialize)]
struct PasswordBody {
    current: String,
    new: String,
    #[serde(default = "yes")]
    sign_out_others: bool,
}

fn yes() -> bool {
    true
}

/// Change the web UI password. The new hash is written to
/// `settings.web_ui.password_hash` in `appconf.yaml` and takes effect at
/// once; other sessions end unless asked not to.
async fn change_password(
    axum::Extension(auth): axum::Extension<Arc<AuthState>>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<PasswordBody>,
) -> ApiResult<serde_json::Value> {
    if let Some(secs) = auth.limiter.locked_for() {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many failed attempts; try again in {secs}s"),
        ));
    }
    let creds = auth.creds.lock_or_recover().clone();
    if !creds.check(&creds.username, &body.current) {
        auth.limiter.fail();
        warn!("web: password change refused, wrong current password");
        return Err(ApiError::new(StatusCode::FORBIDDEN, "the current password is wrong"));
    }
    auth.limiter.succeed();
    if body.new.chars().count() < crate::admin::MIN_PASSWORD_CHARS {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("use at least {} characters", crate::admin::MIN_PASSWORD_CHARS),
        ));
    }
    if body.new == body.current {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "the new password is the current one"));
    }
    let password_hash = crate::admin::hash(&body.new)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let changed_at_ms = crate::stats::now_ms();
    {
        let mut layers = state.config.lock_or_recover();
        let mut cfg = layers.file.clone();
        let web = cfg.settings.web_ui.get_or_insert_with(Default::default);
        web.username = Some(creds.username.clone());
        web.password_hash = Some(password_hash.clone());
        crate::config_store::save(std::path::Path::new(&state.model_config_path), &cfg)
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        layers.file = cfg;
    }
    *auth.creds.lock_or_recover() = WebAuth {
        username: creds.username.clone(),
        secret: Secret::Hash(password_hash),
        source: state.model_config_path.clone(),
        changed_at_ms: Some(changed_at_ms),
    };
    if body.sign_out_others {
        let mine = headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|c| crate::session::cookie_value(c, crate::session::COOKIE));
        auth.sessions.retain_only(mine);
    }
    info!("web: admin password changed");
    state.log_event(crate::dispatcher::LogEvent {
        at: std::time::SystemTime::now(),
        dir: "CTL",
        user: creds.username.clone(),
        model: None,
        backend: None,
        info: "web UI password changed".into(),
        content: None,
        rid: None,
        session: None,
        redacted: false,
    });
    Ok(Json(json!({ "changed_at_ms": changed_at_ms })))
}

// ------------------------------------------------------------ model policy

#[derive(Serialize)]
struct RuleView {
    #[serde(flatten)]
    rule: Rule,
    /// Distinct listed models the rule hits right now (within its scope).
    matches: usize,
}

#[derive(Serialize)]
struct ModelPolicyView {
    mode: Mode,
    rules: Vec<RuleView>,
}

async fn get_models(State(state): State<Arc<AppState>>) -> Json<ModelPolicyView> {
    let policy = state.model_policy();
    let listed: Vec<(String, Vec<String>)> = state
        .backends
        .lock_or_recover()
        .iter()
        .map(|b| (b.url.clone(), b.available_models.iter().cloned().collect()))
        .collect();
    let rules = policy
        .rules
        .iter()
        .map(|r| {
            let mut hit: HashSet<String> = HashSet::new();
            for (url, models) in &listed {
                if !r.backends.is_empty() && !r.backends.contains(url) {
                    continue;
                }
                for m in models {
                    if crate::model_policy::glob_match(&r.pattern, m) {
                        hit.insert(m.to_lowercase());
                    }
                }
            }
            RuleView {
                rule: r.clone(),
                matches: hit.len(),
            }
        })
        .collect();
    Json(ModelPolicyView {
        mode: policy.mode,
        rules,
    })
}

#[derive(Deserialize)]
struct ModeBody {
    mode: Mode,
}

async fn set_model_mode(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ModeBody>,
) -> ApiResult<serde_json::Value> {
    state.model_policy.lock_or_recover().set_mode(body.mode)?;
    warn!("web: model policy mode set to {:?}", body.mode);
    ok()
}

#[derive(Deserialize)]
struct NewRule {
    pattern: String,
    action: Action,
    #[serde(default)]
    backends: Vec<String>,
}

async fn add_model_rule(
    State(state): State<Arc<AppState>>,
    Json(body): Json<NewRule>,
) -> ApiResult<Rule> {
    // Scope must name configured backends: a typo would make a rule that
    // silently never applies.
    {
        let backends = state.backends.lock_or_recover();
        if let Some(bad) = body
            .backends
            .iter()
            .find(|u| !backends.iter().any(|b| &b.url == *u))
        {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("unknown backend '{bad}'"),
            ));
        }
    }
    let rule =
        state
            .model_policy
            .lock_or_recover()
            .add_rule(&body.pattern, body.action, body.backends)?;
    info!(
        "web: model rule {} added: {:?} '{}'",
        rule.id, rule.action, rule.pattern
    );
    Ok(Json(rule))
}

async fn delete_model_rule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    state.model_policy.lock_or_recover().delete_rule(&id)?;
    info!("web: model rule {} removed", id);
    ok()
}

#[derive(Deserialize)]
struct ToggleBody {
    model: String,
    served: bool,
}

async fn toggle_model(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ToggleBody>,
) -> ApiResult<serde_json::Value> {
    state
        .model_policy
        .lock_or_recover()
        .set_served(&body.model, body.served)?;
    info!(
        "web: model '{}' {}",
        body.model,
        if body.served { "served" } else { "hidden" }
    );
    ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatcher::LogEvent;

    fn test_state() -> Arc<AppState> {
        test_state_with(test_convlog())
    }

    fn test_state_with(convlog: crate::convlog::ConvLogger) -> Arc<AppState> {
        // A per-test temp-dir config path: the password-change test saves
        // appconf.yaml (and its .bak) there, never into the checkout. The
        // random suffix keeps parallel tests from clobbering one another.
        let conf = std::env::temp_dir().join(format!(
            "omq-web-conf-{}-{}.yaml",
            std::process::id(),
            crate::clients::random_hex(6)
        ));
        Arc::new(AppState::new(
            vec![("http://127.0.0.1:9".to_string(), None)],
            5,
            86400,
            1,
            1,
            conf.to_string_lossy().into_owned(),
            Vec::new(),
            convlog,
            65_536,
            512 * 1024 * 1024,
        ))
    }

    /// A real conversation log in a temp file: the conversation endpoints are
    /// worth testing against the store the dispatcher would fill, not a stub.
    fn test_convlog() -> crate::convlog::ConvLogger {
        let path = std::env::temp_dir().join(format!(
            "omq-web-conv-{}-{}.jsonl",
            std::process::id(),
            crate::clients::random_hex(6)
        ));
        crate::convlog::ConvLogger::start(
            path.to_string_lossy().into_owned(),
            1 << 20,
            1,
            std::time::Duration::from_secs(3600),
        )
        .unwrap()
        .0
    }

    /// Two turns of one agent conversation, as the request path would record it.
    fn record_two_turns(state: &AppState) {
        use crate::convlog::{Begin, MsgMeta, Note, Resp};
        let key = "alice#a3f19c".to_string();
        state.convlog.note(Note::Begin(Box::new(Begin {
            kind: None,
            link: None,
            key: key.clone(),
            rid: 7,
            user: "alice".into(),
            session_label: Some("refactor-auth".into()),
            model: Some("qwen".into()),
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            bytes: Some(10),
            priority: "normal".into(),
            at_ms: 1000,
            queue_ms: Some(1001),
            redacted: false,
            msgs: vec![MsgMeta { role: "user".into(), chars: 4, name: None, calls: vec![] }],
        })));
        state.convlog.note(Note::Stage {
            key: key.clone(),
            rid: 7,
            queue: Some(1001),
            dispatch: Some(1050),
            first_token: None,
        });
        state.convlog.note(Note::ReqText {
            key: key.clone(),
            rid: 7,
            bytes: Some(10),
            text: std::sync::Arc::new("{\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}".into()),
        });
        state.convlog.note(Note::Resp(Box::new(Resp {
            key: key.clone(),
            rid: 7,
            end_ms: 2000,
            first_token: Some(1500),
            status: Some(200),
            resp_bytes: Some(5),
            backend: Some("gpu:11434".into()),
            outcome: "ok".into(),
            prompt_tokens: Some(3),
            completion_tokens: Some(2),
            waste_secs: None,
            redacted: false,
        })));
        state.convlog.note(Note::RespText {
            key: key.clone(),
            rid: 7,
            bytes: Some(5),
            text: std::sync::Arc::new("hello back".into()),
        });
        state.convlog.note(Note::Begin(Box::new(Begin {
            kind: None,
            link: None,
            key: key.clone(),
            rid: 8,
            user: "alice".into(),
            session_label: None,
            model: Some("qwen".into()),
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            bytes: Some(20),
            priority: "high".into(),
            at_ms: 5000,
            queue_ms: Some(5001),
            redacted: false,
            msgs: vec![MsgMeta {
                role: "tool".into(),
                chars: 6,
                name: Some("read_file".into()),
                calls: vec![],
            }],
        })));
        state.convlog.note(Note::ReqText {
            key: key.clone(),
            rid: 8,
            bytes: Some(20),
            text: std::sync::Arc::new("{\"messages\":[\"older\",\"tool result\"]}".into()),
        });
        state.convlog.note(Note::Resp(Box::new(Resp {
            key,
            rid: 8,
            end_ms: 6000,
            first_token: None,
            status: Some(200),
            resp_bytes: Some(5),
            backend: Some("gpu:11434".into()),
            outcome: "ok".into(),
            prompt_tokens: None,
            completion_tokens: None,
            waste_secs: None,
            redacted: false,
        })));
    }

    /// A log path that could not be opened at boot (the usual cause: a container
    /// path that does not exist on this host) has to be visible as such. Before
    /// this, every log row the operator clicked failed with the same message an
    /// evicted conversation produces, which reads as a broken viewer.
    #[tokio::test]
    async fn capture_off_is_named_instead_of_blaming_the_conversation() {
        let state = test_state_with(crate::convlog::ConvLogger::disabled());
        let base = serve(state).await;
        let c = http();

        let cap: serde_json::Value = c
            .get(format!("{base}/ui/api/capture"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cap["log"], false);
        assert!(cap["path"].is_null(), "no path when nothing was opened");

        let r = c
            .get(format!("{base}/ui/api/conv?rid=1"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        let body: serde_json::Value = r.json().await.unwrap();
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("capture is off"),
            "an unopenable row blames capture, not the conversation: {body}"
        );
    }

    #[tokio::test]
    async fn conversation_api_serves_one_conversation_and_per_turn_text() {
        let state = test_state();
        record_two_turns(&state);
        let base = serve(state).await;
        let c = http();

        let list: Vec<serde_json::Value> = c
            .get(format!("{base}/ui/api/convs"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["key"], "alice#a3f19c");
        assert_eq!(list[0]["turns"], 2);
        assert_eq!(list[0]["session_label"], "refactor-auth");
        assert_eq!(list[0]["priority"], "high", "the highest level the conversation saw");
        assert_eq!(list[0]["live"], false);

        let conv: serde_json::Value = c
            .get(format!("{base}/ui/api/conv?session=a3f19c&user=alice"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(conv["v"], crate::convlog::SCHEMA_V);
        assert_eq!(conv["turns"].as_array().unwrap().len(), 2);
        // Turn order is oldest first, and every stage carries its measured time.
        assert_eq!(conv["turns"][0]["rid"], 7);
        assert_eq!(conv["turns"][0]["tl"]["queue"], 1001);
        assert_eq!(conv["turns"][0]["tl"]["dispatch"], 1050);
        assert_eq!(conv["turns"][0]["tl"]["first_token"], 1500);
        assert_eq!(conv["turns"][0]["tl"]["end"], 2000);
        assert_eq!(conv["turns"][1]["tl"]["queue"], 5001);
        assert_eq!(conv["turns"][1]["msgs"][0]["name"], "read_file");
        // The index carries sizes, never text — the viewer fetches per turn.
        assert!(conv["turns"][0].get("content").is_none());
        assert_eq!(conv["turns"][0]["req"]["chars"], 45);

        // Reload re-reads by the key the viewer already holds: `#` is part of the
        // key, so it must survive the query string (it does not — it separates a
        // fragment — hence percent-encoded, as the client encodes it).
        let by_key: serde_json::Value = c
            .get(format!("{base}/ui/api/conv?key=alice%23a3f19c"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(by_key["key"], "alice#a3f19c");
        assert_eq!(by_key["turns"].as_array().unwrap().len(), 2);
        assert_eq!(
            c.get(format!("{base}/ui/api/conv?key=alice%23nope"))
                .basic_auth("admin", Some("pw"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );

        // Per-turn text, by rid, both directions.
        let out: serde_json::Value = c
            .get(format!("{base}/ui/api/conv/content?rid=7&dir=out"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(out["content"], "hello back");
        let req: serde_json::Value = c
            .get(format!("{base}/ui/api/conv/content?session=a3f19c&dir=in"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(req["content"].as_str().unwrap().starts_with("{\"messages\""));

        // Unknown conversation and a turn without text are 404, not empty 200s.
        assert_eq!(
            c.get(format!("{base}/ui/api/conv?session=deadbe"))
                .basic_auth("admin", Some("pw"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            c.get(format!("{base}/ui/api/conv/content?rid=8&dir=out"))
                .basic_auth("admin", Some("pw"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    /// The Logs page's file-backed tail: journal records below the live ring
    /// so a restart opens with history, the requests the ring still holds are
    /// not listed twice, and restored rows announce themselves as history.
    #[tokio::test]
    async fn logs_page_serves_journal_history_under_the_ring() {
        let state = test_state();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        state.log_event(LogEvent {
            at: std::time::UNIX_EPOCH + std::time::Duration::from_millis(now),
            dir: "IN",
            user: "alice".into(),
            model: Some("qwen".into()),
            backend: None,
            info: "POST /v1/chat/completions body=20B".into(),
            content: None,
            rid: Some(3),
            session: Some("a3f19c".into()),
            redacted: false,
        });
        let rec = |at_ms: u64, rid: Option<u64>, outcome: &'static str| crate::stats::Finished {
            at_ms,
            client: "dm".into(),
            key: "main".into(),
            model: "qwen".into(),
            backend: Some("gpu1".into()),
            outcome,
            session: Some("c342d9".into()),
            session_label: Some("nightly".into()),
            priority: "normal",
            rid,
            tools: vec![],
            queue_wait: Some(1.0),
            stage: Default::default(),
            timing: Default::default(),
            session_key: None,
            kind: None,
            link: None,
        };
        state.stats.record(rec(now - 60_000, Some(41), "ok")); // restored history
        state.stats.record(rec(now, Some(3), "ok")); // the live round, not yet evicted
        let base = serve(state).await;
        let page: serde_json::Value = http()
            .get(format!("{base}/ui/api/logs?limit=50"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let list = page["events"].as_array().unwrap();
        assert_eq!(list.len(), 2, "the live event and one restored row: {list:?}");
        assert_eq!(list[0]["dir"], "IN");
        assert!(list[0]["hist"].is_null(), "ring rows are not marked history");
        assert_eq!(list[1]["hist"], true);
        assert_eq!(list[1]["rid"], 41, "restored rows keep their rid (unique across boots)");
        assert_eq!(list[1]["session"], "c342d9");
        assert_eq!(list[1]["session_label"], "nightly");
        assert_eq!(list[1]["info"], "ok", "the outcome is the compact segment");
        assert_eq!(
            list[1]["has_content"], false,
            "the journal remembers rid 41, but no conversation in the chain can open it: \
             the row is inert history, not a click that 404s"
        );
    }

    /// The window is the window: `/api/logs?since=` returns EVERY journalled
    /// row in it — the old 300-event default and 1000-cap collapsed a 48 h
    /// window to its newest few conversations, which is the bug this
    /// endpoint's shape was built to kill.
    #[tokio::test]
    async fn logs_since_returns_the_whole_window_not_a_count() {
        let state = test_state();
        let now = crate::stats::now_ms();
        let rec = |at_ms: u64, rid: u64| crate::stats::Finished {
            at_ms,
            client: "dm".into(),
            key: "main".into(),
            model: "qwen".into(),
            backend: Some("gpu1".into()),
            outcome: "ok",
            session: Some(format!("s{rid}")),
            session_label: None,
            priority: "normal",
            rid: Some(rid),
            tools: vec![],
            queue_wait: Some(1.0),
            stage: Default::default(),
            timing: Default::default(),
            session_key: None,
            kind: None,
            link: None,
        };
        // 1200 finished requests in one 20-minute burst: both the default
        // answer (300) and the retired MAX_LOG_LIMIT (1000) would truncate it.
        for i in 0..1_200u64 {
            state.stats.record(rec(now - 20 * 60_000 + i * 500, 100 + i));
        }
        let base = serve(state).await;
        let c = http();
        let page: serde_json::Value = c
            .get(format!("{base}/ui/api/logs?since={}", now - 48 * 3_600_000))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let events = page["events"].as_array().unwrap();
        assert_eq!(events.len(), 1_200, "the 48 h window holds every journalled row");
        assert_eq!(page["truncated"], false, "a served window is not a truncated one");
        assert_eq!(page["oldest_ms"].as_u64().unwrap(), now - 20 * 60_000);
        // A narrow since really cuts: asking for the last minute alone
        // returns none of them.
        let narrow: serde_json::Value = c
            .get(format!("{base}/ui/api/logs?since={}", now - 60_000))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(narrow["events"].as_array().unwrap().is_empty());
    }

    /// The 1 s Logs poll has to be free while nothing happens: the endpoint
    /// hands out a change-tag and answers it with a bodyless 304 until the
    /// window actually changes.
    #[tokio::test]
    async fn logs_etag_answers_304_while_the_window_is_unchanged() {
        let state = test_state();
        let ev = |info: &'static str| LogEvent {
            at: std::time::SystemTime::now(),
            dir: "IN",
            user: "u".into(),
            model: None,
            backend: None,
            info: info.into(),
            content: None,
            rid: None,
            session: None,
            redacted: false,
        };
        state.log_event(ev("e1"));
        let base = serve(state.clone()).await;
        let c = http();
        let res = c
            .get(format!("{base}/ui/api/logs?since=0"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap();
        let tag = res
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(tag.len() > 8, "a change-tag is handed out: {tag}");
        let res = c
            .get(format!("{base}/ui/api/logs?since=0"))
            .basic_auth("admin", Some("pw"))
            .header(header::IF_NONE_MATCH, &tag)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(res.text().await.unwrap(), "", "304 carries no body");
        // A new event changes the tag: the cached list is stale again.
        state.log_event(ev("e2"));
        let res = c
            .get(format!("{base}/ui/api/logs?since=0"))
            .basic_auth("admin", Some("pw"))
            .header(header::IF_NONE_MATCH, &tag)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_ne!(
            res.headers().get(header::ETAG).unwrap().to_str().unwrap(),
            tag,
            "a changed window carries a new tag"
        );
    }

    /// The poll that matters: the Logs page asks for its window every second,
    /// and each ask starts a second later than the last. Sliding `since` must
    /// not count as a change — if it did, the 304 path existed only in tests
    /// and a 48 h window was re-sent whole once a second in production.
    #[tokio::test]
    async fn logs_etag_survives_the_window_sliding_forward() {
        let state = test_state();
        let ev = LogEvent {
            at: std::time::SystemTime::now(),
            dir: "IN",
            user: "u".into(),
            model: None,
            backend: None,
            info: "e1".into(),
            content: None,
            rid: None,
            session: None,
            redacted: false,
        };
        state.log_event(ev);
        let base = serve(state.clone()).await;
        let c = http();
        let first = c
            .get(format!("{base}/ui/api/logs?since=1000&win=86400000"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap();
        let tag = first
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        drop(first);
        // One second later, same width, nothing new: the answer is "nothing".
        let again = c
            .get(format!("{base}/ui/api/logs?since=1001&win=86400000"))
            .basic_auth("admin", Some("pw"))
            .header(header::IF_NONE_MATCH, &tag)
            .send()
            .await
            .unwrap();
        assert_eq!(
            again.status(),
            StatusCode::NOT_MODIFIED,
            "a poll one second later must not re-fetch the window"
        );
        // A different width is a different question, unchanged data or not.
        let wider = c
            .get(format!("{base}/ui/api/logs?since=1&win=90000000"))
            .basic_auth("admin", Some("pw"))
            .header(header::IF_NONE_MATCH, &tag)
            .send()
            .await
            .unwrap();
        assert_eq!(wider.status(), StatusCode::OK, "48 h is not 25 h");
    }


    /// The audit slot of the change-tag is the only route a flagged request has
    /// to an operator whose window has otherwise stopped moving: the verdict
    /// arrives after the row was served, from a background job, and if it does
    /// not change the answer, the flag is invisible until something else moves.
    #[tokio::test]
    async fn logs_etag_notices_a_verdict_that_landed_after_the_row() {
        let state = test_state();
        let ev = LogEvent {
            at: std::time::SystemTime::now(),
            dir: "IN",
            user: "u".into(),
            model: None,
            backend: None,
            info: "e1".into(),
            content: None,
            rid: Some(7),
            session: None,
            redacted: false,
        };
        state.log_event(ev);
        let base = serve(state.clone()).await;
        let c = http();
        let first = c
            .get(format!("{base}/ui/api/logs?since=0"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap();
        let tag = first
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        drop(first);
        state.audit.remember(
            7,
            &[crate::audit::Flag {
                kind: crate::audit::Kind::Injection,
                src: crate::audit::Src::User,
                by: "rule:test".into(),
                prob: None,
                category: None,
                detail: None,
                excerpt: None,
                bad: true,
            }],
        );
        let after = c
            .get(format!("{base}/ui/api/logs?since=0"))
            .basic_auth("admin", Some("pw"))
            .header(header::IF_NONE_MATCH, &tag)
            .send()
            .await
            .unwrap();
        assert_eq!(
            after.status(),
            StatusCode::OK,
            "a verdict that lands late must not hide behind a 304"
        );
    }

    /// A journalled row the conversation chain cannot open renders as inert
    /// history (has_content false), and the 404 left for the race names
    /// retention and the knob that fixes it — the operator's complaint was
    /// rows the list showed and the viewer refused to open.
    #[tokio::test]
    async fn journalled_rows_only_offer_the_log_they_can_open() {
        let state = test_state();
        record_two_turns(&state); // rids 7 and 8 sit in the conversation log
        let now = crate::stats::now_ms();
        let rec = |at_ms: u64, rid: Option<u64>| crate::stats::Finished {
            at_ms,
            client: "alice".into(),
            key: "main".into(),
            model: "qwen".into(),
            backend: Some("gpu1".into()),
            outcome: "ok",
            session: Some("a3f19c".into()),
            session_label: None,
            priority: "normal",
            rid,
            tools: vec![],
            queue_wait: Some(1.0),
            stage: Default::default(),
            timing: Default::default(),
            session_key: None,
            kind: None,
            link: None,
        };
        state.stats.record(rec(now - 60_000, Some(7))); // the chain resolves it
        state.stats.record(rec(now - 59_000, Some(41))); // its conversation is gone
        let base = serve(state.clone()).await;
        let c = http();
        let page: serde_json::Value = c
            .get(format!("{base}/ui/api/logs?since=0"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let events = page["events"].as_array().unwrap();
        let by_rid = |rid: u64| {
            events
                .iter()
                .find(|e| e["rid"] == serde_json::json!(rid))
                .unwrap_or_else(|| panic!("rid {rid} in {events:?}"))
        };
        assert_eq!(by_rid(7)["has_content"], true, "a resolvable row is a clickable row");
        assert_eq!(by_rid(41)["has_content"], false, "an aged-out row is inert history");
        assert!(page["chain_start_ms"].is_number(), "the answer states the chain reach");
        assert!(page["indexing"].is_boolean());
        // The bare "no such conversation" 404 is gone: let the (empty-file)
        // boot scan settle so the indexing answer cannot precede, then ask
        // for a conversation that is really not there.
        for _ in 0..100 {
            if !state.convlog.indexing() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!state.convlog.indexing(), "the boot scan finished");
        let r = c
            .get(format!("{base}/ui/api/conv?rid=999"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let err: serde_json::Value = r.json().await.unwrap();
        let msg = err["error"].as_str().unwrap_or_default();
        assert!(msg.contains("request_log_max_files"), "the 404 names the knob: {msg}");
    }

    /// A restart keeps its promises: after the writer has flushed and a
    /// FRESH logger (the stand-in for a fresh process) opens the same file,
    /// `conv?rid=` and `conv?key=` still resolve — cold, from the retained
    /// files only. A link copied before the restart still opens its log.
    #[tokio::test]
    async fn conv_endpoints_resolve_history_in_a_fresh_process() {
        let path = std::env::temp_dir().join(format!(
            "omq-web-cold-{}-{}.jsonl",
            std::process::id(),
            crate::clients::random_hex(6)
        ));
        let path = path.to_string_lossy().into_owned();
        {
            // Boot 1: record a conversation, let the writer flush it (a short
            // idle interval so the line reaches the file), then let go.
            let logger = crate::convlog::ConvLogger::start(
                path.clone(),
                1 << 20,
                1,
                std::time::Duration::from_millis(20),
            )
            .unwrap()
            .0;
            let state = test_state_with(logger);
            record_two_turns(&state);
            for _ in 0..50 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 0 {
                    break;
                }
            }
            assert!(std::fs::metadata(&path).is_ok_and(|m| m.len() > 0), "the line flushed");
        }
        // Boot 2: a fresh logger over the same file — the only knowledge left
        // is what the retained files hold.
        let logger =
            crate::convlog::ConvLogger::start(path.clone(), 1 << 20, 1, std::time::Duration::from_secs(3600))
                .unwrap()
                .0;
        let state = test_state_with(logger);
        let base = serve(state).await;
        let c = http();
        for q in ["conv?rid=7", "conv?key=alice%23a3f19c"] {
            let res = c
                .get(format!("{base}/ui/api/{q}"))
                .basic_auth("admin", Some("pw"))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{q} resolves cold");
            let conv: serde_json::Value = res.json().await.unwrap();
            assert_eq!(conv["key"], "alice#a3f19c", "{q}");
            assert_eq!(conv["turns"].as_array().unwrap().len(), 2, "{q}");
        }
        let _ = std::fs::remove_file(&path);
    }

    async fn serve(state: Arc<AppState>) -> String {
        let app = router(WebAuth::plain("admin", "pw")).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        base
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn sign_in_flow() {
        let base = serve(test_state()).await;
        let c = http();
        let post_json = |path: &str, body: serde_json::Value, cookie: Option<&str>| {
            let mut r = c
                .post(format!("{base}/ui/api/{path}"))
                .header("X-OllamaMQ-UI", "1")
                .json(&body);
            if let Some(ck) = cookie {
                r = r.header(header::COOKIE, ck);
            }
            r.send()
        };

        // Pages redirect to the sign-in form; API calls get a bare 401 (no
        // `WWW-Authenticate`, so browsers show no credentials dialog).
        let res = c.get(format!("{base}/ui/")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()[header::LOCATION], "/ui/login?next=%2Fui%2F");
        let res = c.get(format!("{base}/ui/api/state")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(res.headers().get(header::WWW_AUTHENTICATE).is_none());

        // The sign-in page and its assets are public, with security headers.
        let res = c.get(format!("{base}/ui/login")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get(header::CONTENT_SECURITY_POLICY).is_some());
        assert!(res.text().await.unwrap().contains("Sign in"));
        for asset in ["login.js", "app.css", "favicon.svg", "apple-touch-icon.png"] {
            let res = c.get(format!("{base}/ui/{asset}")).send().await.unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{asset}");
        }

        // Wrong password, then the right one: a session cookie.
        let res = post_json(
            "login",
            json!({"username": "admin", "password": "nope"}),
            None,
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = post_json(
            "login",
            json!({"username": "admin", "password": "pw"}),
            None,
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let set = res.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Strict") && set.contains("Path=/ui")
        );
        assert!(!set.contains("Secure"), "plain http: no Secure flag");
        let cookie = set.split(';').next().unwrap().to_string();

        let res = c
            .get(format!("{base}/ui/"))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let me: serde_json::Value = c
            .get(format!("{base}/ui/api/me"))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(me["user"], "admin");

        // Login also needs the UI header (no cross-site sign-in).
        let res = c
            .post(format!("{base}/ui/api/login"))
            .json(&json!({"username": "admin", "password": "pw"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Signing out ends the session.
        let res = post_json("logout", json!({}), Some(&cookie)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = c
            .get(format!("{base}/ui/api/state"))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // Scripts can still use basic auth with the same credentials.
        let res = c
            .get(format!("{base}/ui/api/state"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v: serde_json::Value = res.json().await.unwrap();
        assert_eq!(v["backends"][0]["url"], "http://127.0.0.1:9");
    }

    #[tokio::test]
    async fn sign_in_is_rate_limited() {
        let base = serve(test_state()).await;
        let c = http();
        let try_login = |pw: &'static str| {
            c.post(format!("{base}/ui/api/login"))
                .header("X-OllamaMQ-UI", "1")
                .json(&json!({"username": "admin", "password": pw}))
                .send()
        };
        for _ in 0..10 {
            assert_eq!(
                try_login("bad").await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
        // Locked: even the right password is refused for now.
        assert_eq!(
            try_login("pw").await.unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[tokio::test]
    async fn mutations_need_the_ui_header_and_manage_clients() {
        let state = test_state();
        let base = serve(state.clone()).await;
        let c = http();
        let post = |path: &str, body: serde_json::Value, csrf: bool| {
            let mut req = c
                .post(format!("{base}/ui/api/{path}"))
                .basic_auth("admin", Some("pw"))
                .json(&body);
            if csrf {
                req = req.header("X-OllamaMQ-UI", "1");
            }
            req.send()
        };

        let res = post("clients", json!({"name": "alice"}), false)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(state.clients.lock_or_recover().len(), 0);

        let res = post("clients", json!({"name": "alice", "note": "n"}), true)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = post("clients", json!({"name": "alice"}), true)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let res = post("clients", json!({"name": "bad name"}), true)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let generated: serde_json::Value =
            post("clients/alice/keys", json!({"label": "gen"}), true)
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        assert!(generated["key"].as_str().unwrap().starts_with("omq-"));
        let res = post("clients/alice/keys", json!({"key": "my-own-key-1"}), true)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = post("clients/nobody/keys", json!({}), true).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Rename the generated key's label through the PATCH route.
        let kid = generated["id"].as_str().unwrap();
        let res = c
            .patch(format!("{base}/ui/api/keys/{kid}"))
            .basic_auth("admin", Some("pw"))
            .header("X-OllamaMQ-UI", "1")
            .json(&json!({"label": "work laptop"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let labels: Vec<String> = state
            .clients
            .lock_or_recover()
            .list()
            .iter()
            .flat_map(|cl| cl.keys.iter().map(|k| k.key.label.clone()))
            .collect();
        assert!(labels.contains(&"work laptop".to_string()));
        // The capture switch rides the same PATCH route: off for this key means
        // its requests log entries without body text.
        let gkey = generated["key"].as_str().unwrap();
        assert!(!state.clients.lock_or_recover().resolve(gkey).unwrap().redact_log);
        let res = c
            .patch(format!("{base}/ui/api/keys/{kid}"))
            .basic_auth("admin", Some("pw"))
            .header("X-OllamaMQ-UI", "1")
            .json(&json!({ "redact_log": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(state.clients.lock_or_recover().resolve(gkey).unwrap().redact_log);
        // A key no longer changes owner: that is a delete plus a new key.
        let res = c
            .patch(format!("{base}/ui/api/keys/{kid}"))
            .basic_auth("admin", Some("pw"))
            .header("X-OllamaMQ-UI", "1")
            .json(&json!({ "client": "bob" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let res = c
            .patch(format!("{base}/ui/api/keys/{kid}"))
            .basic_auth("admin", Some("pw"))
            .header("X-OllamaMQ-UI", "1")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            state
                .clients
                .lock_or_recover()
                .resolve("my-own-key-1")
                .unwrap()
                .client,
            "alice"
        );

        // The capture switch is a client setting the UI can flip.
        let res = c
            .patch(format!("{base}/ui/api/clients/alice"))
            .basic_auth("admin", Some("pw"))
            .header("X-OllamaMQ-UI", "1")
            .json(&json!({ "redact_log": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        {
            let mut st = state.clients.lock_or_recover();
            assert!(st.list().iter().any(|cl| cl.name == "alice" && cl.redact_log));
            assert!(
                st.resolve("my-own-key-1").unwrap().redact_log,
                "a client with capture off covers its keys"
            );
        }
        // And back off again.
        let res = c
            .patch(format!("{base}/ui/api/clients/alice"))
            .basic_auth("admin", Some("pw"))
            .header("X-OllamaMQ-UI", "1")
            .json(&json!({ "redact_log": false }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(!state
            .clients
            .lock_or_recover()
            .resolve("my-own-key-1")
            .unwrap()
            .redact_log);

        // Revoke the generated key.
        let id = generated["id"].as_str().unwrap();
        let res = c
            .delete(format!("{base}/ui/api/keys/{id}"))
            .basic_auth("admin", Some("pw"))
            .header("X-OllamaMQ-UI", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let list: serde_json::Value = c
            .get(format!("{base}/ui/api/clients"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list[0]["keys"].as_array().unwrap().len(), 1);
        assert_eq!(list[0]["keys"][0]["key"], "my-own-key-1");

        // VIP toggle reaches the shared state and is saved on the client.
        let res = post("users/alice/vip", json!({}), true).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(state.vip_user.lock_or_recover().as_deref(), Some("alice"));
        assert_eq!(
            state.clients.lock_or_recover().priority().0.as_deref(),
            Some("alice")
        );

        // Blocking a registered client disables it; it lists as a disabled
        // client, not a blocked id.
        let res = post("users/alice/block", json!({}), true).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let st: serde_json::Value = c
            .get(format!("{base}/ui/api/state"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(st["blocked"]["clients"], json!(["alice"]));
        assert_eq!(st["blocked"]["users"], json!([]));
        let res = post(
            "blocked/unblock",
            json!({"kind": "client", "value": "alice"}),
            true,
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(!state.clients.lock_or_recover().is_disabled("alice"));

        // Renaming moves the saved VIP with the client.
        let res = c
            .patch(format!("{base}/ui/api/clients/alice"))
            .basic_auth("admin", Some("pw"))
            .header("X-OllamaMQ-UI", "1")
            .json(&json!({"name": "alice2"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(state.vip_user.lock_or_recover().as_deref(), Some("alice2"));
    }

    #[tokio::test]
    async fn model_rules_api() {
        let state = test_state();
        state.backends.lock_or_recover()[0].available_models = Arc::new(
            ["gemma3:27b", "llava:13b", "nomic-embed-text:latest"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        let base = serve(state.clone()).await;
        let c = http();
        let send = |m: reqwest::Method, path: &str, body: serde_json::Value| {
            c.request(m, format!("{base}/ui/api/{path}"))
                .basic_auth("admin", Some("pw"))
                .header("X-OllamaMQ-UI", "1")
                .json(&body)
                .send()
        };
        let get = |path: &str| {
            c.get(format!("{base}/ui/api/{path}"))
                .basic_auth("admin", Some("pw"))
                .send()
        };

        let res = send(
            reqwest::Method::POST,
            "models/rules",
            json!({"pattern": "*embed*", "action": "hide"}),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = send(
            reqwest::Method::POST,
            "models/rules",
            json!({"pattern": "x", "action": "hide", "backends": ["http://nope:1"]}),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let pol: serde_json::Value = get("models").await.unwrap().json().await.unwrap();
        assert_eq!(pol["mode"], "deny");
        assert_eq!(pol["rules"][0]["matches"], 1);

        let res = send(
            reqwest::Method::POST,
            "models/toggle",
            json!({"model": "llava:13b", "served": false}),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let st: serde_json::Value = get("state").await.unwrap().json().await.unwrap();
        assert_eq!(
            st["backends"][0]["models_hidden"],
            json!(["llava:13b", "nomic-embed-text:latest"])
        );

        // Allow mode with no allow rules: only the toggled-on model is served.
        let res = send(
            reqwest::Method::PUT,
            "models/mode",
            json!({"mode": "allow"}),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        send(
            reqwest::Method::POST,
            "models/toggle",
            json!({"model": "gemma3:27b", "served": true}),
        )
        .await
        .unwrap();
        let p = state.model_policy();
        assert!(p.served("gemma3:27b", "http://127.0.0.1:9"));
        assert!(!p.served("llava:13b", "http://127.0.0.1:9"));
        // A pattern hide rule cannot be switched off from one model.
        let res = send(
            reqwest::Method::POST,
            "models/toggle",
            json!({"model": "nomic-embed-text:latest", "served": true}),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn log_sequence_numbers_survive_eviction() {
        let state = test_state();
        let ev = |info: String| LogEvent {
            at: std::time::SystemTime::now(),
            dir: "IN",
            user: "u".into(),
            model: None,
            backend: None,
            info,
            content: Some(Arc::new("body".into())),
            rid: None,
            session: None,
            redacted: false,
        };
        // Overflow the ring so the oldest events are evicted.
        for i in 0..350 {
            state.log_event(ev(format!("e{i}")));
        }
        let base = serve(state).await;
        let c = http();
        let get = |path: String| {
            c.get(format!("{base}/ui/api/{path}"))
                .basic_auth("admin", Some("pw"))
                .send()
        };

        let page: serde_json::Value = get("logs?limit=5".into())
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let list = page["events"].as_array().unwrap();
        assert_eq!(list.len(), 5);
        assert_eq!(list[0]["info"], "e349");
        assert_eq!(list[0]["seq"], 349);
        assert!(list[0].get("content").is_none());

        let one: serde_json::Value = get("logs/349".into()).await.unwrap().json().await.unwrap();
        assert_eq!(one["info"], "e349");
        assert_eq!(one["content"], "body");
        assert_eq!(
            get("logs/3".into()).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn log_records_carry_rid_and_session() {
        // The web UI pairs IN/OUT into rounds and finds a conversation's
        // previous round from these two fields on every record.
        let state = test_state();
        state.log_event(LogEvent {
            at: std::time::SystemTime::now(),
            dir: "IN",
            user: "u".into(),
            model: Some("m".into()),
            backend: None,
            info: "POST /v1/chat/completions body=10B".into(),
            content: Some(Arc::new("{\"messages\":[]}".into())),
            rid: Some(7),
            session: Some("a3f19c".into()),
            redacted: false,
        });
        state.log_event(LogEvent {
            at: std::time::SystemTime::now(),
            dir: "CTL",
            user: "admin".into(),
            model: None,
            backend: None,
            info: "ctl".into(),
            content: None,
            rid: None,
            session: None,
            redacted: false,
        });
        let base = serve(state).await;
        let page: serde_json::Value = http()
            .get(format!("{base}/ui/api/logs"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let list = page["events"].as_array().unwrap();
        assert_eq!(list[1]["rid"], 7);
        assert_eq!(list[1]["session"], "a3f19c");
        // Absent when unset — old-shaped records keep their exact shape.
        assert!(list[0].get("rid").is_none());
        assert!(list[0].get("session").is_none());
    }

    /// Signed-in calls with basic auth, as scripts make them.
    fn api(c: &reqwest::Client, method: reqwest::Method, base: &str, path: &str) -> reqwest::RequestBuilder {
        c.request(method, format!("{base}/ui/api/{path}"))
            .basic_auth("admin", Some("pw"))
            .header("X-OllamaMQ-UI", "1")
    }

    #[tokio::test]
    async fn settings_change_live_and_report_their_source() {
        let state = test_state();
        let base = serve(state.clone()).await;
        let c = http();
        let put = |body: serde_json::Value| api(&c, reqwest::Method::PUT, &base, "settings").json(&body).send();

        let res = put(json!({"set": {"timeout": 42, "model_aliases": {"short": "llama3"}}})).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let view: serde_json::Value = res.json().await.unwrap();
        let timeout = view["fields"].as_array().unwrap().iter().find(|f| f["key"] == "timeout").unwrap().clone();
        // The UI persists into appconf.yaml (single-store design), so the
        // changed value reports its source as "file".
        assert_eq!((timeout["value"].clone(), timeout["source"].clone()), (json!(42), json!("file")));
        assert_eq!(state.timeout.load(Ordering::Relaxed), 42);
        assert!(state.model_aliases.lock_or_recover().contains_key("short"));

        // Startup-only settings wait for a restart.
        let view: serde_json::Value = put(json!({"set": {"max_body_bytes": 4096}})).await.unwrap().json().await.unwrap();
        assert_eq!(view["restart_pending"], json!(["max_body_bytes"]));

        // Unknown names, wrong types and unsafe values are refused.
        for bad in [json!({"port": 1}), json!({"timeout": "soon"}), json!({"timeout": 0})] {
            let res = put(json!({ "set": bad })).await.unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
        assert_eq!(state.timeout.load(Ordering::Relaxed), 42);

        // `null` falls back to the file/default value.
        put(json!({"set": {"timeout": null}})).await.unwrap();
        assert_eq!(state.timeout.load(Ordering::Relaxed), 300);

        // A value pinned on the command line cannot be changed here.
        state.config.lock_or_recover().pinned_by.insert("timeout", "--timeout".into());
        let res = put(json!({"set": {"timeout": 7}})).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        // The metrics token is never shown, only that it is set.
        let view: serde_json::Value = put(json!({"set": {"metrics_token": "abc123"}})).await.unwrap().json().await.unwrap();
        let tok = view["fields"].as_array().unwrap().iter().find(|f| f["key"] == "metrics_token").unwrap().clone();
        assert_ne!(tok["value"], json!("abc123"));
        assert_eq!(state.metrics_token.lock_or_recover().as_deref(), Some("abc123"));
    }

    #[tokio::test]
    async fn backends_are_added_named_disabled_and_removed() {
        let state = test_state();
        // As at startup with no backends configured: the running one.
        state.config.lock_or_recover().fallback_backends = vec![crate::config::Backend {
            url: "http://127.0.0.1:9".into(),
            name: None,
            disabled: false,
            token: None,
            kind: None,
            redact_log: false,
        }];
        let base = serve(state.clone()).await;
        let c = http();
        let call = |m: reqwest::Method, path: &str, body: serde_json::Value| api(&c, m, &base, path).json(&body).send();
        let post = reqwest::Method::POST;
        let put = reqwest::Method::PUT;

        let res = call(post.clone(), "settings/backends", json!({"url": "10.0.0.9:8000/", "name": " gpu-a "})).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        {
            let b = state.backends.lock_or_recover();
            let added = b.iter().find(|b| b.url == "http://10.0.0.9:8000").expect("added");
            assert_eq!(added.name.as_deref(), Some("gpu-a"));
        }
        assert!(!state.backends.lock_or_recover()[0].disabled, "first edit keeps the running backend");
        // Same URL again, or a second backend with the same name: refused.
        let res = call(post.clone(), "settings/backends", json!({"url": "http://10.0.0.9:8000"})).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let res = call(put.clone(), "settings/backends", json!({"url": "http://127.0.0.1:9", "name": "GPU-A"})).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // Disable: offline and never probed.
        call(put.clone(), "settings/backends", json!({"url": "http://10.0.0.9:8000", "disabled": true, "name": null})).await.unwrap();
        {
            let b = state.backends.lock_or_recover();
            let x = b.iter().find(|b| b.url == "http://10.0.0.9:8000").unwrap();
            assert!(x.disabled && !x.is_online);
            assert_eq!(x.name, None);
        }

        // Remove: gone from the list, kept (disabled) until a restart.
        let view: serde_json::Value = call(post, "settings/backends/remove", json!({"url": "http://10.0.0.9:8000"}))
            .await.unwrap().json().await.unwrap();
        let row = view["backends"].as_array().unwrap().iter().find(|b| b["url"] == "http://10.0.0.9:8000").unwrap().clone();
        assert_eq!((row["removed"].clone(), row["disabled"].clone()), (json!(true), json!(true)));
    }

    #[tokio::test]
    async fn password_change_takes_effect_and_ends_other_sessions() {
        let base = serve(test_state()).await;
        let c = http();
        let login = || async {
            let res = c.post(format!("{base}/ui/api/login")).header("X-OllamaMQ-UI", "1")
                .json(&json!({"username": "admin", "password": "pw"})).send().await.unwrap();
            res.headers()[header::SET_COOKIE].to_str().unwrap().split(';').next().unwrap().to_string()
        };
        let (mine, other) = (login().await, login().await);
        let change = |current: &str, new: &str| {
            c.post(format!("{base}/ui/api/account/password"))
                .header("X-OllamaMQ-UI", "1")
                .header(header::COOKIE, &mine)
                .json(&json!({"current": current, "new": new}))
                .send()
        };
        assert_eq!(change("wrong", "a-much-longer-password").await.unwrap().status(), StatusCode::FORBIDDEN);
        assert_eq!(change("pw", "short").await.unwrap().status(), StatusCode::BAD_REQUEST);
        assert_eq!(change("pw", "a-much-longer-password").await.unwrap().status(), StatusCode::OK);

        let state_with = |user: &str, pw: &str| c.get(format!("{base}/ui/api/state")).basic_auth(user, Some(pw)).send();
        assert_eq!(state_with("admin", "pw").await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert_eq!(state_with("admin", "a-much-longer-password").await.unwrap().status(), StatusCode::OK);
        let with_cookie = |ck: &str| c.get(format!("{base}/ui/api/state")).header(header::COOKIE, ck).send();
        assert_eq!(with_cookie(&mine).await.unwrap().status(), StatusCode::OK);
        assert_eq!(with_cookie(&other).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        let me: serde_json::Value = c.get(format!("{base}/ui/api/me")).header(header::COOKIE, &mine)
            .send().await.unwrap().json().await.unwrap();
        assert!(me["password_source"].as_str().unwrap().ends_with(".yaml"));
        assert!(me["password_changed_at_ms"].as_u64().is_some());
    }

    #[test]
    fn hash_secret_beats_plaintext() {
        let h = crate::admin::hash("hashed-secret").unwrap();
        // A hash wins over any plaintext password, whichever set the name.
        let a = WebAuth::resolve(
            None,
            Some((h.clone(), "appconf.yaml")),
            Some(("plain-secret".to_string(), "OLLAMA_MQ_UI_PASSWORD")),
        )
        .unwrap();
        assert_eq!(a.username, "admin");
        assert!(a.check("admin", "hashed-secret"));
        assert!(!a.check("admin", "plain-secret"));
        // No hash: the env plaintext applies with its user name.
        let b = WebAuth::resolve(
            Some("operator".into()),
            None,
            Some(("plain-secret".to_string(), "OLLAMA_MQ_UI_PASSWORD")),
        )
        .unwrap();
        assert!(b.check("operator", "plain-secret") && !b.check("admin", "plain-secret"));
        // Nothing at all: web UI off.
        assert!(WebAuth::resolve(None, None, None).is_none());
    }
}
