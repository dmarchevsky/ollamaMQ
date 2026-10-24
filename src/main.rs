use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use clap::Parser;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod admin;
mod auth;
mod clients;
mod config;
mod config_store;
mod control;
mod dispatcher;
mod lock;
mod metrics;
mod model_policy;
mod convlog;
mod session;
mod settings;
mod snapshot;
mod stats;
mod tui;
mod web;

use crate::control::{admin_model_load, admin_model_unload, admin_models_state};
use crate::dispatcher::{AppState, proxy_handler, run_worker};
use crate::lock::LockExt;

use std::io::IsTerminal;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Port to listen on (overrides appconf.yaml `settings.port`)
    #[arg(short, long)]
    port: Option<u16>,

    /// Host/interface to bind to (overrides appconf.yaml `settings.host`)
    #[arg(short = 'H', long)]
    host: Option<String>,

    /// Backend server URLs (e.g., Ollama, LM Studio) — comma-separated list.
    /// Overrides appconf.yaml `backends`.
    #[arg(
        short,
        long,
        value_delimiter = ',',
        alias = "ollama-urls"
    )]
    backend_urls: Option<Vec<String>>,

    /// Request timeout in seconds (overrides appconf.yaml `settings.timeout`)
    #[arg(short, long)]
    timeout: Option<u64>,

    /// Disable TUI dashboard
    #[arg(long)]
    no_tui: bool,

    /// Allow all routes (enable fallback proxy; overrides
    /// appconf.yaml `settings.allow_all_routes`)
    #[arg(long, default_value_t = false)]
    allow_all_routes: bool,

    /// How long (seconds) models stay loaded after a model-control "load"
    /// request (sent as Ollama `keep_alive`; LM Studio loads are unaffected
    /// beyond its own TTL). Ollama's own default is only 5 minutes.
    /// Overrides appconf.yaml `settings.load_keep_alive`.
    #[arg(long)]
    load_keep_alive: Option<i64>,

    /// Path to the central configuration file: backends, settings and the
    /// models to load onto them. Applied at startup; reload with 'r' in the TUI.
    #[arg(short = 'c', long, default_value = config::DEFAULT_CONFIG_FILE)]
    model_config: String,
}

struct TuiState {
    visible: bool,
    toggle_notify: Arc<Notify>,
}

/// Build the 401 response with JSON content type and a Bearer challenge hint.
fn unauthorized_response() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("content-type", "application/json")
        .header("www-authenticate", "Bearer")
        .body(axum::body::Body::from("{\"error\":\"unauthorized\"}"))
        .unwrap()
}

/// Optional API-key auth middleware. If no key is configured (`api_key` is
/// None), all requests pass through untouched. When one is, the request must
/// present it or the key of an enabled client.
async fn auth_middleware(
    State((api_key, state)): State<(Option<String>, Arc<AppState>)>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = api_key else {
        return next.run(request).await;
    };

    let Some(provided) = auth::extract_api_key(request.headers()) else {
        return unauthorized_response();
    };

    let client_ok = || {
        state
            .clients
            .lock_or_recover()
            .resolve(&provided)
            .is_some_and(|c| !c.disabled)
    };
    if auth::constant_time_eq(&provided, &expected) || client_ok() {
        next.run(request).await
    } else {
        unauthorized_response()
    }
}

/// Routes the proxy does not know: proxied when `allow_all_routes` is on,
/// else 404 like any other router.
async fn fallback_handler(
    State(state): State<Arc<AppState>>,
    connect: axum::extract::ConnectInfo<SocketAddr>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
    uri: axum::extract::OriginalUri,
    body: bytes::Bytes,
) -> Response {
    if !state.allow_all_routes.load(std::sync::atomic::Ordering::Relaxed) {
        return StatusCode::NOT_FOUND.into_response();
    }
    proxy_handler(State(state), connect, method, headers, uri, body)
        .await
        .into_response()
}

/// `GET /metrics`: Prometheus text format, behind an optional bearer token.
fn metrics_handler(
    state: Arc<AppState>,
    headers: axum::http::HeaderMap,
    token: Option<&str>,
) -> Response {
    if let Some(expected) = token {
        let ok = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| auth::constant_time_eq(t.trim(), expected));
        if !ok {
            return unauthorized_response();
        }
    }
    Response::builder()
        .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
        .body(axum::body::Body::from(metrics::render(&state)))
        .unwrap()
}

/// Normalize a backend URL from the CLI or config: strip trailing slashes
/// and add `http://` when no scheme is given.
#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Central config file: backends, settings and models. CLI flags win
    // whenever they are explicitly given; a missing file is fine (defaults).
    let file_cfg = match config::load_config(&args.model_config) {
        Ok(cfg) => cfg,
        Err(e) => {
            if !e.contains("No such file") && std::path::Path::new(&args.model_config).exists() {
                eprintln!("Warning: {}", e);
            }
            info!("No model config loaded from '{}': {}", args.model_config, e);
            config::AppConfig::default()
        }
    };
    // Old side stores (`settings.json` overlay, `admin.json`) merge into
    // appconf.yaml once, then retire to `.bak`. Failing loudly beats running
    // with an overlay or a password silently ignored.
    let file_cfg = match settings::merge_legacy_stores(&args.model_config, file_cfg) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    // Values given on the command line or in the environment win over both
    // files; the web UI shows them as locked.
    let env_nonempty = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut pinned = config::Settings::default();
    let mut pinned_by = std::collections::BTreeMap::new();
    if let Some(v) = args.port {
        pinned.port = Some(v);
        pinned_by.insert("port", "--port".to_string());
    }
    if let Some(v) = args.host.clone() {
        pinned.host = Some(v);
        pinned_by.insert("host", "--host".to_string());
    }
    if let Some(v) = args.timeout {
        pinned.timeout = Some(v);
        pinned_by.insert("timeout", "--timeout".to_string());
    }
    if let Some(v) = args.load_keep_alive {
        pinned.load_keep_alive = Some(v);
        pinned_by.insert("load_keep_alive", "--load-keep-alive".to_string());
    }
    if args.allow_all_routes {
        pinned.allow_all_routes = Some(true);
        pinned_by.insert("allow_all_routes", "--allow-all-routes".to_string());
    }
    if let Some(v) = env_nonempty("OLLAMA_MQ_METRICS_TOKEN") {
        pinned.metrics_token = Some(v);
        pinned_by.insert("metrics_token", "OLLAMA_MQ_METRICS_TOKEN".to_string());
    }

    // Store files (clients, model rules) live next to appconf.yaml unless a
    // path says otherwise; logs go to the working directory. See
    // `settings::store_path`.
    let config_dir = settings::config_dir(&args.model_config);
    let mut resolved = std::collections::BTreeMap::new();
    let mut legacy_notes = Vec::new();
    let mut store_file = |key: &'static str, name: Option<String>, default: &str| -> String {
        let name = name.unwrap_or_else(|| default.to_string());
        let (path, legacy) = settings::store_path(&name, &config_dir, std::path::Path::new(""));
        let path = path.to_string_lossy().into_owned();
        if legacy {
            legacy_notes.push(format!(
                "{key}: using {path} from the working directory; move it next to {} to keep the configuration in one folder",
                args.model_config
            ));
        }
        resolved.insert(key, path.clone());
        path
    };

    let mut layers = settings::Layers {
        file: file_cfg.clone(),
        pinned,
        pinned_by,
        backends_from_cli: args.backend_urls.is_some(),
        fallback_backends: Vec::new(),
        startup: config::Settings::default(),
        resolved: Default::default(),
    };
    let eff = layers.effective();
    layers.startup = eff.clone();

    let port = eff.port.unwrap_or(11435);
    let host = eff.host.clone().unwrap_or_else(|| "127.0.0.1".to_string());
    let timeout = eff.timeout.unwrap_or(300);
    let load_keep_alive = eff.load_keep_alive.unwrap_or(86400);
    let stuck_timeout = eff.stuck_timeout.unwrap_or(60);
    // Default the global per-backend cap to the highest configured per-model
    // concurrency so `max_concurrent_requests: N` isn't silently capped at 1
    // when `settings.max_concurrent_per_backend` is omitted.
    let max_concurrent_per_backend = eff.max_concurrent_per_backend.unwrap_or_else(|| {
        file_cfg
            .models
            .iter()
            .map(|m| m.max_concurrent_requests)
            .max()
            .unwrap_or(1)
    });
    let request_log_path = eff
        .request_log_path
        .clone()
        .unwrap_or_else(|| crate::convlog::DEFAULT_LOG_FILE.to_string());
    let request_log_max_bytes = eff.request_log_max_bytes.unwrap_or(10 * 1024 * 1024);
    let request_log_max_files = eff.request_log_max_files.unwrap_or(5);
    let request_log_flush =
        std::time::Duration::from_secs(eff.request_log_flush_secs.unwrap_or(60));
    let log_content_limit = eff
        .log_content_limit
        .map(|v| v.max(1024))
        .unwrap_or(1024 * 1024);
    // Request bodies are buffered whole (a queued request may wait before a
    // backend takes it), so the body limit is a per-request memory commitment.
    // It used to be a flat 1 GiB on every route.
    let max_body_bytes = eff.max_body_bytes.unwrap_or(64 * 1024 * 1024);
    let max_queued_bytes = eff.max_queued_bytes.unwrap_or(512 * 1024 * 1024);
    let clients_file = store_file("clients_file", eff.clients_file.clone(), clients::DEFAULT_CLIENTS_FILE);
    let models_file = store_file("models_file", eff.models_file.clone(), model_policy::DEFAULT_MODELS_FILE);
    layers.resolved = resolved;
    // Web UI credentials: a hash in appconf.yaml (`password_hash`, written by
    // the UI's "Change password") wins; otherwise the env password, then the
    // plaintext one. Nothing set at all = UI off.
    let web_cfg = file_cfg.settings.web_ui.clone().unwrap_or_default();
    let web_auth = web::WebAuth::resolve(
        env_nonempty("OLLAMA_MQ_UI_USER").or(web_cfg.username.filter(|u| !u.is_empty())),
        web_cfg
            .password_hash
            .filter(|h| !h.is_empty())
            .map(|h| (h, "appconf.yaml")),
        env_nonempty("OLLAMA_MQ_UI_PASSWORD")
            .map(|p| (p, "OLLAMA_MQ_UI_PASSWORD"))
            .or(web_cfg.password.filter(|p| !p.is_empty()).map(|p| (p, "appconf.yaml"))),
    );

    // The backend list: `--backend-urls` (fixed), else the UI's or the file's.
    let backend_list: Vec<config::Backend> = match &args.backend_urls {
        Some(urls) => urls
            .iter()
            .map(|u| config::Backend {
                url: u.clone(),
                name: None,
                disabled: false,
                token: None,
                kind: None,
                redact_log: false,
            })
            .collect(),
        None => layers.backend_list(),
    };
    let backend_list = if backend_list.is_empty() {
        vec![config::Backend {
            url: "http://localhost:11434".to_string(),
            name: None,
            disabled: false,
            token: None,
            kind: None,
            redact_log: false,
        }]
    } else {
        backend_list
    };
    if args.backend_urls.is_some() || layers.backend_list().is_empty() {
        // `--backend-urls`, or the built-in default: what the UI starts from.
        layers.fallback_backends = backend_list.clone();
    }
    // `kind:` per backend, in list order (pinned before the first probe).
    let backend_kinds: Vec<Option<String>> = backend_list.iter().map(|b| b.kind.clone()).collect();
    let backends: Vec<(String, Option<String>)> = backend_list
        .iter()
        .map(|b| (settings::normalize_url(&b.url), b.token.clone()))
        .collect();

    // Determine if we should run TUI
    let use_tui = !args.no_tui && std::io::stdout().is_terminal();

    // Keep the guard alive for the duration of main
    let _guard: Option<tracing_appender::non_blocking::WorkerGuard>;

    if use_tui {
        let file_appender = tracing_appender::rolling::never(".", "ollamamq.log");
        let (non_blocking, g) = tracing_appender::non_blocking(file_appender);
        _guard = Some(g);

        tracing_subscriber::fmt()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .init();
    } else {
        _guard = None;
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug")),
            )
            .init();
    }

    // Start the size-rotated conversation log. Each candidate path gets its
    // directories created, and an unusable configured path falls back to the
    // store directory next to appconf.yaml: a config that still names a
    // container path (`/data/…`) on a host without one used to run with capture
    // silently off, so no conversation could be looked up later. The writer
    // task handle is kept alive for the process lifetime (same pattern as
    // `_guard` above).
    let store_dir = std::path::Path::new(&args.model_config)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let mut opened = None;
    let mut attempts = Vec::new();
    for cand in crate::convlog::ConvLogger::open_candidates(&request_log_path, &store_dir) {
        match crate::convlog::ConvLogger::start(
            cand.clone(),
            request_log_max_bytes,
            request_log_max_files,
            request_log_flush,
        ) {
            Ok(pair) => {
                if cand != request_log_path {
                    tracing::warn!(
                        "conversation log: {request_log_path} is unusable here, writing to {cand} instead"
                    );
                }
                opened = Some(pair);
                break;
            }
            Err(e) => attempts.push(format!("{cand} ({e})")),
        }
    }
    let (convlog, _convlog_guard) = match opened {
        Some(x) => x,
        None => {
            tracing::error!("request log disabled — tried {}", attempts.join(", "));
            (
                crate::convlog::ConvLogger::disabled(),
                tokio::task::spawn(async {}),
            )
        }
    };

    let state = Arc::new(AppState::new(
        backends,
        timeout,
        load_keep_alive,
        stuck_timeout,
        max_concurrent_per_backend,
        args.model_config.clone(),
        file_cfg.models,
        convlog,
        log_content_limit,
        max_queued_bytes,
    ));

    for note in &legacy_notes {
        info!("{note}");
    }

    // A clients file that exists but cannot be read is fatal: running with an
    // empty store would answer every keyed client as a stranger, and the
    // first change made in the web UI would overwrite the file.
    match clients::ClientStore::load(&clients_file) {
        Ok(store) => {
            info!("Loaded {} client(s) from {}", store.len(), clients_file);
            *state.clients.lock_or_recover() = store;
            state.apply_client_store();
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }

    // Same rule for the model policy: an unreadable file must not silently
    // start serving every model the operator hid.
    match model_policy::ModelPolicyStore::load(&models_file) {
        Ok(store) => {
            let p = store.snapshot();
            info!(
                "Loaded model policy from {} ({:?} mode, {} rule(s))",
                models_file,
                p.mode,
                p.rules.len()
            );
            *state.model_policy.lock_or_recover() = store;
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }

    // Live settings, names and enabled state of backends; then keep every
    // layer so the web UI can show and change them.
    settings::apply(&state, &eff);
    settings::apply_backends(&state, &backend_list);
    if !state.model_aliases.lock_or_recover().is_empty() {
        info!("{} model alias(es) loaded", state.model_aliases.lock_or_recover().len());
    }
    if !eff.detect_backend_slots.unwrap_or(true) {
        info!("Backend slot detection off: every backend uses max_concurrent_per_backend");
    }
    *state.config.lock_or_recover() = layers;

    // Pin the `kind:`-configured API families now, BEFORE the health loop
    // starts: a Laya-style backend answers nothing that identifies it, so a
    // probe round running first would classify it `???` and leave it there for
    // a whole tick.
    state.apply_backend_kinds(&backend_kinds);

    let worker_state = state.clone();
    tokio::spawn(async move {
        run_worker(worker_state).await;
    });

    // Apply appconf.yaml once backends have been probed by the health loop
    // (first probe round runs immediately; wait up to 30 s for it).
    {
        let st = state.clone();
        tokio::spawn(async move {
            if st.model_config.lock_or_recover().is_empty() {
                return;
            }
            for _ in 0..60 {
                let probed = {
                    let backends = st.backends.lock_or_recover();
                    // "Probed" means "answered at least one endpoint", not "has
                    // a non-empty family set". A System One backend is perfectly
                    // routable while answering nothing that used to classify a
                    // family, and a backend that answers nothing at all is never
                    // a load target either way — so insisting on
                    // `api_type != Unknown` made a CLM-only deployment sit out
                    // this whole 30 s loop before its model config was applied.
                    backends
                        .iter()
                        .all(|b| b.is_online || !b.api_type.is_empty())
                };
                if probed {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let n = control::apply_model_config(&st);
            info!("Model config applied ({} entries)", n);
        });
    }

    // Optional API-key auth: enabled only when OLLAMA_MQ_API_KEY is set (non-empty).
    let api_key = std::env::var("OLLAMA_MQ_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());

    let mut app = Router::new()
        // Ollama API Endpoints (Explicitly listed)
        .route("/", any(proxy_handler))
        .route("/api/generate", any(proxy_handler))
        .route("/api/chat", any(proxy_handler))
        .route("/api/embed", any(proxy_handler))
        .route("/api/embeddings", any(proxy_handler))
        .route("/api/tags", any(proxy_handler))
        .route("/api/show", any(proxy_handler))
        .route("/api/create", any(proxy_handler))
        .route("/api/copy", any(proxy_handler))
        .route("/api/delete", any(proxy_handler))
        .route("/api/pull", any(proxy_handler))
        .route("/api/push", any(proxy_handler))
        // Model-layer uploads are the one route that legitimately carries
        // hundreds of megabytes, so it keeps its own allowance. A route layer
        // sits inside the router-wide one below, so this value wins here.
        .route(
            "/api/blobs/{digest}",
            any(proxy_handler).layer(axum::extract::DefaultBodyLimit::max(
                dispatcher::BLOB_BODY_LIMIT,
            )),
        )
        .route("/api/ps", any(proxy_handler))
        .route("/api/version", any(proxy_handler))
        // OpenAI Compatible Endpoints
        .route("/v1/chat/completions", any(proxy_handler))
        .route("/v1/completions", any(proxy_handler))
        .route("/v1/embeddings", any(proxy_handler))
        .route("/v1/models", any(proxy_handler))
        .route("/v1/models/{model}", any(proxy_handler))
        // System One decision-model API (CLM / Laya / Kev). Pass-through like
        // every other proxy route — the body is never parsed — and the route
        // table routes it to a System One backend and nowhere else.
        .route("/v1/systemone", any(proxy_handler))
        // Olla-style prefixes (`proxy_handler` strips them).
        .route("/olla/openai/{*rest}", any(proxy_handler))
        .route("/olla/proxy/{*rest}", any(proxy_handler))
        // Local admin API (model load/unload control) — never proxied to
        // backends, protected by the same optional API-key auth.
        .route("/admin/models", get(admin_models_state))
        .route("/admin/models/load", post(admin_model_load))
        .route("/admin/models/unload", post(admin_model_unload));

    // Unknown routes: proxied while `allow_all_routes` is on (live setting).
    app = app.fallback(fallback_handler);

    // Protect all proxy routes (including the fallback) with optional API-key auth.
    // /health is registered separately below and stays unauthenticated.
    let app = app
        .layer(middleware::from_fn_with_state(
            (api_key, state.clone()),
            auth_middleware,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(max_body_bytes))
        .with_state(state.clone());

    if state.metrics_token.lock_or_recover().is_some() {
        info!("/metrics requires a bearer token");
    }
    let mut app = Router::new()
        .route("/health", get(|| async { "OK" }))
        // Outside the API-key layer, like /health: Prometheus scrapes it
        // with its own (optional) token.
        .route(
            "/metrics",
            get(
                |State(state): State<Arc<AppState>>, headers: axum::http::HeaderMap| async move {
                    let token = state.metrics_token.lock_or_recover().clone();
                    metrics_handler(state, headers, token.as_deref())
                },
            ),
        )
        .merge(app);
    // The web UI has its own (basic) auth, so it sits outside the API-key layer.
    match web_auth {
        Some(auth) => {
            info!("Web UI enabled at /ui (user '{}', password from {})", auth.username, auth.source);
            app = app.merge(web::router(auth));
        }
        None => info!("Web UI disabled (set OLLAMA_MQ_UI_PASSWORD or settings.web_ui.password)"),
    }
    let app = app.with_state(state.clone());

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!("Dispatcher running on http://{}", addr);

    if use_tui {
        let tui_state = Arc::new(Mutex::new(TuiState {
            visible: true,
            toggle_notify: Arc::new(Notify::new()),
        }));

        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        // Run TUI on the main thread
        tui_loop(tui_state, state).await;
    } else {
        // Just run the server on the main thread
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    }
}

async fn tui_loop(tui_state: Arc<Mutex<TuiState>>, state: Arc<AppState>) {
    let mut dashboard = tui::TuiDashboard::new();
    let toggle_notify = Arc::new(tui_state.lock_or_recover().toggle_notify.clone());

    loop {
        let visible = {
            let tui_state = tui_state.lock_or_recover();
            tui_state.visible
        };

        if visible {
            match dashboard.run(&state) {
                Ok(continue_loop) => {
                    if !continue_loop {
                        return;
                    }
                }
                Err(e) => {
                    eprintln!("TUI error: {}", e);
                    return;
                }
            }
        } else {
            toggle_notify.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Arc<AppState> {
        Arc::new(AppState::new(
            vec![("http://127.0.0.1:9".to_string(), None)],
            5,
            86400,
            1,
            1,
            "appconf.yaml".into(),
            Vec::new(),
            crate::convlog::ConvLogger::disabled(),
            65_536,
            512 * 1024 * 1024,
        ))
    }

    #[test]
    fn metrics_token_is_optional() {
        let st = state();
        let open = metrics_handler(st.clone(), axum::http::HeaderMap::new(), None);
        assert_eq!(open.status(), StatusCode::OK);

        let denied = metrics_handler(st.clone(), axum::http::HeaderMap::new(), Some("s3cret"));
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let mut h = axum::http::HeaderMap::new();
        h.insert("authorization", "Bearer s3cret".parse().unwrap());
        let ok = metrics_handler(st, h, Some("s3cret"));
        assert_eq!(ok.status(), StatusCode::OK);
        assert!(
            ok.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/plain; version=0.0.4")
        );
    }
}
