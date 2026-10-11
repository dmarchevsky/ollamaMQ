//! Web UI at `/ui`: the TUI's functions in a browser, plus client and API-key
//! management. Behind HTTP basic auth; off unless a password is configured.
//!
//! The page is static (embedded from `src/web/`) and talks to the JSON API
//! under `/ui/api/`. Every mutating call must carry `X-OllamaMQ-UI: 1`: a
//! custom header forces a CORS preflight, so another site cannot ride the
//! browser's cached basic-auth credentials to change anything.

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
use crate::snapshot;

const INDEX_HTML: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const APP_CSS: &str = include_str!("web/app.css");
const DEMO_JS: &str = include_str!("web/demo.js");

/// Upper bound on the logs one `/ui/api/logs` call returns (the ring buffer
/// itself holds fewer).
const MAX_LOG_LIMIT: usize = 1000;

/// Header every mutating API call must carry (see the module docs).
const CSRF_HEADER: &str = "x-ollamamq-ui";

const CSP: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; \
                   img-src 'self' data:; frame-ancestors 'none'";

/// Basic-auth credentials for the web UI.
#[derive(Clone)]
pub struct WebAuth {
    pub username: String,
    pub password: String,
}

/// Routes for `/ui` and `/ui/api/*`, all behind basic auth.
pub fn router(auth: WebAuth) -> Router<Arc<AppState>> {
    let api = Router::new()
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
        .nest("/ui/api", api)
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(Arc::new(auth), basic_auth))
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

async fn basic_auth(State(auth): State<Arc<WebAuth>>, request: Request, next: Next) -> Response {
    let ok = basic_credentials(request.headers()).is_some_and(|(user, pass)| {
        // Evaluate both so the response time does not reveal which one failed.
        let user_ok = constant_time_eq(&user, &auth.username);
        let pass_ok = constant_time_eq(&pass, &auth.password);
        user_ok & pass_ok
    });
    if !ok {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"ollamaMQ\", charset=\"UTF-8\"",
            )
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from("{\"error\":\"unauthorized\"}"))
            .unwrap();
    }
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
    async fn basic_auth_guards_everything() {
        let base = serve(test_state()).await;
        let c = http();

        let res = c.get(format!("{base}/ui/")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(
            res.headers()[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .starts_with("Basic ")
        );
        let res = c
            .get(format!("{base}/ui/api/state"))
            .basic_auth("admin", Some("wrong"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = c
            .get(format!("{base}/ui/"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .is_ok()
        );
        assert!(res.text().await.unwrap().contains("ollamaMQ"));

        let res = c
            .get(format!("{base}/ui"))
            .basic_auth("admin", Some("pw"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::PERMANENT_REDIRECT);

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
