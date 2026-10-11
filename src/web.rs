//! Web UI at `/ui`: the TUI's functions in a browser, plus client and API-key
//! management. Off unless a password is configured.
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
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{info, warn};

use crate::auth::{basic_credentials, constant_time_eq};
use crate::clients::{ClientPatch, StoreError};
use crate::control::{self, ControlAction, LoadOptions};
use crate::dispatcher::AppState;
use crate::lock::LockExt;
use crate::model_policy::{Action, Mode, Rule};
use crate::snapshot;

const INDEX_HTML: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const APP_CSS: &str = include_str!("web/app.css");
const DEMO_JS: &str = include_str!("web/demo.js");
const LOGIN_HTML: &str = include_str!("web/login.html");
const LOGIN_JS: &str = include_str!("web/login.js");

/// Reachable without signing in: the login page and what it needs.
const PUBLIC_PATHS: &[&str] = &[
    "/ui",
    "/ui/login",
    "/ui/login.js",
    "/ui/app.css",
    "/ui/api/login",
    "/ui/api/logout",
];

/// Upper bound on the logs one `/ui/api/logs` call returns (the ring buffer
/// itself holds fewer).
const MAX_LOG_LIMIT: usize = 1000;

/// Header every mutating API call must carry (see the module docs).
const CSRF_HEADER: &str = "x-ollamamq-ui";

const CSP: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; \
                   img-src 'self' data:; frame-ancestors 'none'";

/// The web UI's credentials (one admin user).
#[derive(Clone)]
pub struct WebAuth {
    pub username: String,
    pub password: String,
}

impl WebAuth {
    fn check(&self, user: &str, pass: &str) -> bool {
        // Evaluate both so the response time does not reveal which one failed.
        let user_ok = constant_time_eq(user, &self.username);
        let pass_ok = constant_time_eq(pass, &self.password);
        user_ok & pass_ok
    }
}

struct AuthState {
    creds: WebAuth,
    sessions: crate::session::Sessions,
    limiter: crate::session::LoginLimiter,
}

/// Routes for `/ui` and `/ui/api/*`; all but the login page need a session
/// (or basic-auth credentials).
pub fn router(auth: WebAuth) -> Router<Arc<AppState>> {
    let auth = Arc::new(AuthState {
        creds: auth,
        sessions: Default::default(),
        limiter: Default::default(),
    });
    let api = Router::new()
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/me", get(me))
        .route("/state", get(get_state))
        .route("/logs", get(list_logs))
        .route("/logs/{seq}", get(get_log))
        .route("/users/{id}/{op}", post(user_action))
        .route("/blocked/unblock", post(unblock))
        .route("/backends/{idx}/{op}", post(model_control))
        .route("/config/reload", post(reload_config))
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
            axum::routing::patch(move_key).delete(delete_key),
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
            "/ui/login.js",
            get(|| async { asset(LOGIN_JS, "text/javascript; charset=utf-8") }),
        )
        .nest("/ui/api", api)
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(auth.clone(), require_auth))
        .layer(axum::Extension(auth))
        .layer(middleware::from_fn(security_headers))
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
    basic_credentials(headers)
        .filter(|(u, p)| auth.creds.check(u, p))
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
    elapsed_secs: f64,
    /// Time to first byte; `None` while the backend has not answered yet.
    ttfb_secs: Option<f64>,
    /// Generated tokens so far (estimated from the stream).
    tokens: u64,
    /// Generation speed since the first byte.
    tokens_per_sec: Option<f64>,
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
    online: bool,
    api_type: &'static str,
    active: usize,
    max: u32,
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
            online: b.is_online,
            api_type: b.api_type.display(),
            active: b.active_requests,
            max: state.max_concurrent_per_backend,
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
                let gen_secs = r.first_byte.map(|f| f.elapsed().as_secs_f64());
                RunView {
                    elapsed_secs: r.started.elapsed().as_secs_f64(),
                    ttfb_secs: ttfb,
                    tokens_per_sec: gen_secs
                        .filter(|s| *s >= 1.0 && r.tokens > 0)
                        .map(|s| r.tokens as f64 / s),
                    tokens: r.tokens,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

fn log_view(seq: u64, ev: &crate::dispatcher::LogEvent, with_content: bool) -> LogView {
    LogView {
        seq,
        at_ms: ev
            .at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        dir: ev.dir,
        user: ev.user.clone(),
        model: ev.model.clone(),
        backend: ev.backend.clone(),
        info: ev.info.clone(),
        has_content: ev.content.as_deref().is_some_and(|c| !c.is_empty()),
        content: if with_content {
            ev.content.as_deref().map(|c| c.to_string())
        } else {
            None
        },
    }
}

#[derive(Deserialize)]
struct LogQuery {
    limit: Option<usize>,
}

/// Newest first, without content (fetch one event for that).
async fn list_logs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LogQuery>,
) -> Json<Vec<LogView>> {
    let limit = q.limit.unwrap_or(300).min(MAX_LOG_LIMIT);
    let logs = state.logs.lock_or_recover();
    let base = state.logs_evicted.load(Ordering::Relaxed);
    Json(
        logs.iter()
            .enumerate()
            .rev()
            .take(limit)
            .map(|(i, ev)| log_view(base + i as u64, ev, false))
            .collect(),
    )
}

async fn get_log(State(state): State<Arc<AppState>>, Path(seq): Path<u64>) -> ApiResult<LogView> {
    let logs = state.logs.lock_or_recover();
    let base = state.logs_evicted.load(Ordering::Relaxed);
    seq.checked_sub(base)
        .and_then(|i| logs.get(i as usize))
        .map(|ev| Json(log_view(seq, ev, true)))
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "log event has expired"))
}

// ------------------------------------------------------- users & blocking

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
        // Like the TUI's 'u' on a user row: lift both blocks.
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
    control::reload_model_config(&state)
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
}

async fn add_key(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<NewKey>,
) -> ApiResult<crate::clients::ApiKey> {
    let key = state
        .clients
        .lock_or_recover()
        .add_key(&name, body.key.as_deref(), &body.label)?;
    // The key itself is never logged.
    info!("web: key {} added to client '{}'", key.id, name);
    Ok(Json(key))
}

#[derive(Deserialize)]
struct MoveKey {
    client: String,
}

async fn move_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<MoveKey>,
) -> ApiResult<serde_json::Value> {
    state
        .clients
        .lock_or_recover()
        .move_key(&id, &body.client)?;
    info!("web: key {} moved to client '{}'", id, body.client);
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
    if !auth.creds.check(&body.username, &body.password) {
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
    Json(json!({ "user": authenticated(&auth, &headers) }))
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
        Arc::new(AppState::new(
            vec![("http://127.0.0.1:9".to_string(), None)],
            5,
            86400,
            1,
            1,
            "appconf.yaml".into(),
            Vec::new(),
            crate::reqlog::RequestLogger::disabled(),
            65_536,
            512 * 1024 * 1024,
        ))
    }

    async fn serve(state: Arc<AppState>) -> String {
        let app = router(WebAuth {
            username: "admin".into(),
            password: "pw".into(),
        })
        .with_state(state);
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
        for asset in ["login.js", "app.css"] {
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
        assert_eq!(
            state
                .clients
                .lock_or_recover()
                .resolve("my-own-key-1")
                .unwrap()
                .client,
            "alice"
        );

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

        let list: Vec<serde_json::Value> = get("logs?limit=5".into())
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
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
}
