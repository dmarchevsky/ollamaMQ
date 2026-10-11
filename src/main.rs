use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::{any, get, post},
};
use clap::Parser;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod auth;
mod clients;
mod config;
mod control;
mod dispatcher;
mod lock;
mod metrics;
mod model_policy;
mod reqlog;
mod session;
mod snapshot;
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
fn normalize_backend_url(url: String) -> String {
    let trimmed = url.trim_end_matches('/').to_string();
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        format!("http://{}", trimmed)
    } else {
        trimmed
    }
}

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

    let port = args.port.or(file_cfg.settings.port).unwrap_or(11435);
    let host = args
        .host
        .or(file_cfg.settings.host)
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let timeout = args.timeout.or(file_cfg.settings.timeout).unwrap_or(300);
    let load_keep_alive = args
        .load_keep_alive
        .or(file_cfg.settings.load_keep_alive)
        .unwrap_or(86400);
    let stuck_timeout = file_cfg.settings.stuck_timeout.unwrap_or(60);
    // Default the global per-backend cap to the highest configured per-model
    // concurrency so `max_concurrent_requests: N` isn't silently capped at 1
    // when `settings.max_concurrent_per_backend` is omitted.
    let max_concurrent_per_backend = file_cfg
        .settings
        .max_concurrent_per_backend
        .unwrap_or_else(|| {
            file_cfg
                .models
                .iter()
                .map(|m| m.max_concurrent_requests)
                .max()
                .unwrap_or(1)
        });
    let request_log_path = file_cfg
        .settings
        .request_log_path
        .clone()
        .unwrap_or_else(|| "ollamamq-requests.jsonl".to_string());
    let request_log_max_bytes = file_cfg.settings.request_log_max_bytes.unwrap_or(10 * 1024 * 1024);
    let request_log_max_files = file_cfg.settings.request_log_max_files.unwrap_or(5);
    let log_content_limit = file_cfg
        .settings
        .log_content_limit
        .map(|v| v.max(1024))
        .unwrap_or(65_536);
    // Request bodies are buffered whole (a queued request may wait before a
    // backend takes it), so the body limit is a per-request memory commitment.
    // It used to be a flat 1 GiB on every route.
    let max_body_bytes = file_cfg.settings.max_body_bytes.unwrap_or(64 * 1024 * 1024);
    let max_queued_bytes = file_cfg
        .settings
        .max_queued_bytes
        .unwrap_or(512 * 1024 * 1024);
    let allow_all_routes =
        args.allow_all_routes || file_cfg.settings.allow_all_routes.unwrap_or(false);
    let clients_file = file_cfg
        .settings
        .clients_file
        .clone()
        .unwrap_or_else(|| clients::DEFAULT_CLIENTS_FILE.to_string());
    let models_file = file_cfg
        .settings
        .models_file
        .clone()
        .unwrap_or_else(|| model_policy::DEFAULT_MODELS_FILE.to_string());
    // Web UI credentials: env vars win over appconf.yaml; no password = off.
    let web_cfg = file_cfg.settings.web_ui.clone().unwrap_or_default();
    let env_nonempty = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let metrics_token = env_nonempty("OLLAMA_MQ_METRICS_TOKEN")
        .or(file_cfg.settings.metrics_token.clone().filter(|t| !t.is_empty()));
    let web_auth = env_nonempty("OLLAMA_MQ_UI_PASSWORD")
        .or(web_cfg.password.filter(|p| !p.is_empty()))
        .map(|password| web::WebAuth {
            username: env_nonempty("OLLAMA_MQ_UI_USER")
                .or(web_cfg.username.filter(|u| !u.is_empty()))
                .unwrap_or_else(|| "admin".to_string()),
            password,
        });

    // `kind:` per backend, in config order, collected before `file_cfg.backends`
    // is consumed below. Only the config file can carry one: `--backend-urls`
    // takes bare URLs, so those backends stay probe-classified (an empty slice
    // pins nothing).
    let backend_kinds: Vec<Option<String>> = if args.backend_urls.is_some() {
        Vec::new()
    } else {
        file_cfg.backends.iter().map(|b| b.kind.clone()).collect()
    };

    // (url, optional auth token) per backend. The CLI --backend-urls always
    // wins; tokens only come from appconf.yaml.
    let backends: Vec<(String, Option<String>)> = if let Some(urls) = args.backend_urls {
        urls.into_iter()
            .map(|u| (normalize_backend_url(u), None))
            .collect()
    } else if file_cfg.backends.is_empty() {
        vec![(
            normalize_backend_url("http://localhost:11434".to_string()),
            None,
        )]
    } else {
        file_cfg
            .backends
            .into_iter()
            .map(|b| (normalize_backend_url(b.url), b.token))
            .collect()
    };

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

    // Start the size-rotated request log; fall back to a no-op logger when
    // the path is unwritable. The writer task handle is kept alive for the
    // process lifetime (same pattern as `_guard` above).
    let (reqlog, _reqlog_guard) = match crate::reqlog::RequestLogger::start(
        request_log_path,
        request_log_max_bytes,
        request_log_max_files,
    ) {
        Ok(x) => x,
        Err(e) => {
            tracing::error!("request log disabled: {e}");
            (
                crate::reqlog::RequestLogger::disabled(),
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
        reqlog,
        log_content_limit,
        max_queued_bytes,
    ));

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

    state.set_model_aliases(file_cfg.settings.model_aliases.as_ref());
    if !state.model_aliases.lock_or_recover().is_empty() {
        info!("{} model alias(es) loaded", state.model_aliases.lock_or_recover().len());
    }
    state.ip_identity.store(
        file_cfg.settings.ip_identity.unwrap_or(true),
        std::sync::atomic::Ordering::Relaxed,
    );
    let detect_slots = file_cfg.settings.detect_backend_slots.unwrap_or(true);
    state
        .detect_slots
        .store(detect_slots, std::sync::atomic::Ordering::Relaxed);
    if !detect_slots {
        info!("Backend slot detection off: every backend uses max_concurrent_per_backend");
    }

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

    // Optional fallback
    if allow_all_routes {
        app = app.fallback(proxy_handler);
    }

    // Protect all proxy routes (including the fallback) with optional API-key auth.
    // /health is registered separately below and stays unauthenticated.
    let app = app
        .layer(middleware::from_fn_with_state(
            (api_key, state.clone()),
            auth_middleware,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(max_body_bytes))
        .with_state(state.clone());

    if metrics_token.is_some() {
        info!("/metrics requires a bearer token");
    }
    let mut app = Router::new()
        .route("/health", get(|| async { "OK" }))
        // Outside the API-key layer, like /health: Prometheus scrapes it
        // with its own (optional) token.
        .route(
            "/metrics",
            get(
                move |State(state): State<Arc<AppState>>, headers: axum::http::HeaderMap| {
                    let token = metrics_token.clone();
                    async move { metrics_handler(state, headers, token.as_deref()) }
                },
            ),
        )
        .merge(app);
    // The web UI has its own (basic) auth, so it sits outside the API-key layer.
    match web_auth {
        Some(auth) => {
            info!("Web UI enabled at /ui (user '{}')", auth.username);
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
            crate::reqlog::RequestLogger::disabled(),
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
