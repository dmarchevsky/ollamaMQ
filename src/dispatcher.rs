use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, State},
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
};
use crate::config::StickyMode;
use serde::Deserializer as _;
use std::hash::Hasher;
use crate::lock::LockExt;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, info, warn};

const BLOCKED_FILE: &str = "blocked_items.json";

#[derive(Serialize, Deserialize, Default)]
struct BlockedConfig {
    ips: HashSet<IpAddr>,
    users: HashSet<String>,
}

/// Minimal view of a request body used to pull out the requested model.
/// Deserializing into this instead of `serde_json::Value` lets serde skip
/// every other field (`IgnoredAny`) rather than allocating the whole tree —
/// request bodies routinely carry hundreds of KB of chat history.
#[derive(Deserialize)]
struct ModelField {
    #[serde(default)]
    model: Option<String>,
}

/// Pull the requested model name out of a request body.
///
/// Only JSON *objects* are considered: serde deserializes a top-level array
/// into `ModelField` positionally (`["llama3"]` would yield `llama3`), so the
/// leading byte is checked first. That also means non-JSON bodies — an
/// `/api/blobs` layer upload, say — are rejected without entering the parser.
fn extract_requested_model(body: &[u8]) -> Option<String> {
    if body
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_none_or(|b| *b != b'{')
    {
        return None;
    }
    serde_json::from_slice::<ModelField>(body)
        .ok()
        .and_then(|m| m.model)
}

pub enum ResponsePart {
    Status(StatusCode, HeaderMap),
    Chunk(Bytes),
    Error(reqwest::Error),
}

pub struct Task {
    pub method: Method,
    pub path: String,
    /// Client identity (ConnectInfo addr string) so OUT request-log records
    /// carry the same user as their IN record.
    pub user: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub responder: mpsc::Sender<ResponsePart>,
    pub requested_model: Option<String>,
    /// Set once a "no backend available" warning has been logged for this
    /// task, so stuck requests are visible without spamming the log.
    pub stuck_warned: bool,
    /// When the request was queued; used to fail fast (503) when no backend
    /// can ever serve it instead of letting the client hang forever.
    pub queued_at: std::time::Instant,
    /// How many cache-hot dispatches this request was held behind by
    /// [`cold_hold`]; bounds the hold-back's delay.
    pub cache_passes: u8,
    /// Label (or id) of the client API key that sent it, for per-key metrics.
    pub key_label: Option<String>,
    /// Sticky-routing key for this request (see [`derive_session_key`]): the
    /// conversation it belongs to, and the backend that should serve it. `None`
    /// means no affinity — the request is placed by load alone.
    pub session_key: Option<SessionKey>,
}

/// Which API flavours a backend speaks — and, on the route side of the
/// scheduler, which flavours a route is allowed to be served by.
///
/// This replaced the enum `BackendApiType { Unknown, Ollama, OpenAi, Both }`.
/// Three families need 2³ subsets, so an enum runs out of variants at the very
/// moment a third family (`SystemOne`, the decision-model servers) is added;
/// a bitset accepts whatever combination results from probing and needs no new
/// plumbing per family. `EMPTY` is the old `Unknown`: nothing detected (yet),
/// which keeps a backend eligible for every family until a probe or real
/// traffic classifies it.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ApiSet(u8);

impl ApiSet {
    /// Nothing known about this backend (the old `BackendApiType::Unknown`).
    pub const EMPTY: ApiSet = ApiSet(0);
    /// Ollama's native `/api/*` API.
    pub const OLLAMA: ApiSet = ApiSet(1 << 0);
    /// OpenAI's `/v1/*` API as served by LM Studio, vLLM and friends.
    pub const OPENAI: ApiSet = ApiSet(1 << 1);
    /// "System One" decision-model servers (CLM, Laya, Kev): `POST
    /// /v1/systemone`, plus a `{"models":[{"name":…}]}` model list on all of
    /// them except Laya. No `/api/*` route at all.
    pub const SYSTEMONE: ApiSet = ApiSet(1 << 2);
    /// Ollama + OpenAI: what a backend serving both flavours is tagged with.
    pub const BOTH: ApiSet = ApiSet(1 << 0 | 1 << 1);
    /// Every known family — the set of a route that any backend may serve.
    pub const ALL: ApiSet = ApiSet(1 << 0 | 1 << 1 | 1 << 2);

    /// One entry per bit, in `members()` order.
    const FAMILIES: [ApiFamily; 3] =
        [ApiFamily::Ollama, ApiFamily::OpenAi, ApiFamily::SystemOne];

    /// Short labels for the dashboard and log lines, indexed by bit pattern.
    const NAMES: [&'static str; 8] = [
        "???",
        "Ollama",
        "OpenAI",
        "O+OA",
        "SystemOne",
        "O+SO",
        "OA+SO",
        "O+OA+SO",
    ];

    const fn bit(family: ApiFamily) -> u8 {
        match family {
            ApiFamily::Ollama => 1 << 0,
            ApiFamily::OpenAi => 1 << 1,
            ApiFamily::SystemOne => 1 << 2,
        }
    }

    pub const fn contains(self, family: ApiFamily) -> bool {
        self.0 & Self::bit(family) != 0
    }

    pub const fn union(self, other: ApiSet) -> ApiSet {
        ApiSet(self.0 | other.0)
    }

    pub const fn intersection(self, other: ApiSet) -> ApiSet {
        ApiSet(self.0 & other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The families in this set, in declaration order.
    pub fn members(self) -> impl Iterator<Item = ApiFamily> {
        Self::FAMILIES.into_iter().filter(move |f| self.contains(*f))
    }

    /// The one family in this set, or `None` when it holds none or several.
    pub fn single(self) -> Option<ApiFamily> {
        let mut members = self.members();
        let one = members.next()?;
        if members.next().is_some() {
            None
        } else {
            Some(one)
        }
    }

    /// Probe-side merge: a probe only ever *adds* flavours it observed.
    pub const fn merge(self, other: ApiSet) -> ApiSet {
        self.union(other)
    }

    pub const fn display(self) -> &'static str {
        Self::NAMES[(self.0 & 0b0111) as usize]
    }

    /// The family pinned by a backend's `kind:` config key (see
    /// [`crate::config::Backend::kind`]). Case/separator insensitive
    /// (`systemone`, `SystemOne`, `system-one` all pin the same family).
    ///
    /// `None` for absent AND for unrecognised values: a key this build doesn't
    /// know must leave the backend to probe classification like any other,
    /// not disable it. Spelling is matched leniently on purpose — a typo here
    /// is invisible in the config file, so an unknown value is logged by the
    /// caller rather than silently pinned to something else.
    pub fn from_kind(kind: Option<&str>) -> Option<ApiSet> {
        let normalised: String = kind?
            .trim()
            .to_lowercase()
            .chars()
            .filter(|c| !matches!(c, '-' | '_' | ' '))
            .collect();
        match normalised.as_str() {
            "systemone" => Some(ApiSet::SYSTEMONE),
            _ => None,
        }
    }
}

/// Old variant spellings of the enum this replaced, so the probe / model-control
/// code (`control.rs`) and its tests keep speaking in flavour names. Note that
/// `supports_control` is now a *containment* test (`api_type.contains(ApiFamily::
/// Ollama)`) rather than `matches!(…, Ollama | Both)`: exact set equality would
/// drop model control from any backend whose merged set grew a second family.
/// These aliases stay for the sites where a whole set is named, not tested.
#[allow(non_upper_case_globals)]
impl ApiSet {
    pub const Unknown: ApiSet = ApiSet::EMPTY;
    pub const Ollama: ApiSet = ApiSet::OLLAMA;
    pub const OpenAi: ApiSet = ApiSet::OPENAI;
    pub const Both: ApiSet = ApiSet::BOTH;
}

/// The backend-side set keeps its historical name across the crate.
pub type BackendApiType = ApiSet;

impl std::fmt::Debug for ApiSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display())
    }
}

/// Which API family a request belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApiFamily {
    Ollama,
    OpenAi,
    /// Decision-model servers (CLM, Laya, Kev): `POST /v1/systemone`.
    SystemOne,
}

impl ApiFamily {
    pub fn display(self) -> &'static str {
        match self {
            ApiFamily::Ollama => "Ollama",
            ApiFamily::OpenAi => "OpenAI",
            ApiFamily::SystemOne => "SystemOne",
        }
    }
}

/// Which backend families may serve `(method, path)`.
///
/// Replaces the prefix rule (`/api/*` → Ollama, `/v1/*` → OpenAI), which sent
/// `/v1/systemone` to LM Studio / vLLM backends that answer it with a 404: a
/// route's family is now looked up rather than guessed from its prefix. Every
/// pre-existing route keeps the family it had — only `/v1/models` (now shared
/// with System One servers, which answer it with a different envelope) and the
/// new `/v1/systemone` differ, and `route_table_keeps_every_registered_routes_family`
/// guards the whole table against accidental changes.
///
/// `ApiSet::ALL` means "any backend may answer", the old `ApiFamily::Unknown`.
/// The method is part of the signature because the route table is keyed by
/// method; no route currently discriminates on it.
pub fn families_for_route(_method: &Method, path: &str) -> ApiSet {
    // Exact routes whose family the old prefix rule got wrong.
    if path == "/v1/systemone" {
        return ApiSet::SYSTEMONE;
    }
    if path.starts_with("/api/") {
        ApiSet::OLLAMA
    } else if path == "/v1/models" || path.starts_with("/v1/models/") {
        // Same endpoint name, two envelopes (see `aggregate_model_list`).
        ApiSet::OPENAI.union(ApiSet::SYSTEMONE)
    } else if path.starts_with("/v1/") {
        // Chat, completions, embeddings — and anything else under `/v1/` that
        // a fallback-routed client sends: unchanged from the prefix rule.
        ApiSet::OPENAI
    } else {
        // `/` and anything unrecognized: unchanged, any backend may answer.
        ApiSet::ALL
    }
}

/// `/api/tags`: Ollama's model list.
const OLLAMA_LIST_SHAPES: &[(&str, &str)] = &[("models", "name")];
/// `/v1/models` is shared by two families with two different envelopes:
/// OpenAI's `{"data":[{"id":…}]}` and the System One servers'
/// `{"models":[{"name":…}]}`. Both are *read*, but the answer is written in the
/// FIRST shape listed — the route's own envelope — and `data`/`id` is first
/// because this route is advertised as OpenAI-compatible and OpenAI SDK clients
/// require `data`. A System One backend ordered first must not re-key it.
const V1_MODELS_SHAPES: &[(&str, &str)] = &[("data", "id"), ("models", "name")];

/// The entry shape(s) of a model-listing endpoint: `(array key, name field)`,
/// **first = the envelope that route answers in**, the rest accepted on input
/// only, or `None` for any path that is not a listing.
fn model_list_shapes(path: &str) -> Option<&'static [(&'static str, &'static str)]> {
    match path {
        "/api/tags" => Some(OLLAMA_LIST_SHAPES),
        "/v1/models" => Some(V1_MODELS_SHAPES),
        _ => None,
    }
}

/// Re-key one listing entry to the envelope's name field (`id` ⇄ `name`),
/// leaving every other field exactly as that backend reported it. Only needed
/// when a `/v1/models` answer arrives in the other family's envelope.
fn retitled_entry(entry: &serde_json::Value, from: &str, to: &str) -> serde_json::Value {
    if from == to {
        return entry.clone();
    }
    let serde_json::Value::Object(map) = entry else {
        return entry.clone();
    };
    let mut map = map.clone();
    if let Some(value) = map.remove(from) {
        map.insert(to.to_string(), value);
    }
    serde_json::Value::Object(map)
}

/// Answer a model listing from EVERY compatible backend at once.
///
/// `/api/tags` and `/v1/models` used to be proxied like any other request, so
/// a client saw whichever single backend the scheduler happened to pick: with
/// several backends holding different models the advertised list changed from
/// one poll to the next, and no client could see everything the proxy is able
/// to route to. Each backend is asked for its own listing and the entries are
/// merged, so per-model metadata (size, digest, details, ...) stays exactly as
/// that backend reported it instead of being synthesized here.
///
/// Entries are deduped by name/id with the first backend winning, and the first
/// answering body is reused so its extra top-level fields survive. Returns
/// `None` when no compatible backend produced a listing AND none of them knows
/// a single model name — the caller then falls back to the normal queued proxy
/// path.
///
/// A backend that cannot answer its listing route at all still contributes. Laya
/// has no `/v1/models` and no `/api/tags` (both are its own 404), so a Laya-only
/// deployment advertised *nothing* and a client — which discovers models by
/// listing them — never sent a request for a model the proxy routes perfectly
/// well. The names such a backend already reports in `available_models` (what
/// the probe discovered plus what `appconf.yaml` declared) are contributed as
/// minimal entries carrying only this route's name field. They are appended
/// after every live entry, and a name some backend did list keeps that backend's
/// rich entry — synthesised entries never shadow size/digest/description.
///
/// A System One backend contributes to `/api/tags` too, a route its family does
/// not serve. It is *not asked* anything there — `/api/*` requests must never
/// reach a System One server — only its known names are listed, on the grounds
/// that this route advertises what the proxy can route to and those models are
/// routable (through `/v1/systemone`). No other family works that way: a backend
/// that cannot serve the route's family contributes nothing, which is what keeps
/// an existing Ollama / LM Studio / OpenAI deployment's listings exactly as they
/// were — they answer their listing route and take the live-body path.
///
/// Both listing envelopes are *read*, but only one is ever *answered*, and which
/// one is decided by the ROUTE, never by whichever backend happens to answer
/// first (that ordering was a real regression: with a System One backend
/// configured first, `GET /v1/models` answered `{"models":[{"name":…}]}` and
/// broke OpenAI SDK clients on a route the README advertises as
/// OpenAI-compatible). So `/v1/models` always emits `{"data":[{"id":…}]}` and
/// `/api/tags` always `{"models":[{"name":…}]}`; an entry read through the other
/// envelope is re-keyed to the answer's name field, and the first answerer's own
/// foreign array key is dropped so the reply is one envelope, not two.
///
/// Backends are queried without the client's headers (as health probes are),
/// so a backend that demands its own credentials on this endpoint contributes
/// nothing to the merge — except via its own configured `token`, which is
/// attached like everywhere else.
async fn aggregate_model_list(
    state: &AppState,
    endpoint: &str,
) -> Option<axum::response::Response> {
    let shapes = model_list_shapes(endpoint)?;
    // The envelope this route answers in: the first declared shape, fixed by
    // the route itself (`/v1/models` → OpenAI's `data`/`id`, `/api/tags` →
    // Ollama's `models`/`name`). Everything after `shapes[0]` is accepted on
    // input only.
    let (out_key, out_field) = *shapes
        .first()
        .expect("a listing endpoint declares at least one shape");
    let route = families_for_route(&Method::GET, endpoint);

    /// The model names we know a backend serves even if it cannot list them,
    /// sorted because `available_models` is a `HashSet` whose iteration order
    /// changes every process run and a listing route must not shuffle.
    fn sorted_names(models: &std::sync::Arc<HashSet<String>>) -> Vec<String> {
        let mut names: Vec<String> = models.iter().cloned().collect();
        names.sort();
        names
    }

    // Backends that get asked for their own listing, and the names of those
    // that must not be asked but have models to advertise anyway (below).
    let mut probed: Vec<(String, Option<String>, Vec<String>)> = Vec::new();
    let mut state_only: Vec<(String, Vec<String>)> = Vec::new();
    // Hidden models are dropped per backend, before de-duplication, so one
    // hidden on one backend still lists when another serves it.
    let policy = state.model_policy();
    {
        let backends = state.backends.lock_or_recover();
        for b in backends.iter() {
            if !b.is_online {
                continue;
            }
            let names = sorted_names(&b.available_models);
            if family_compatible(b, route) {
                probed.push((b.url.clone(), b.token.clone(), names));
            } else if b.api_type.contains(ApiFamily::SystemOne) && !names.is_empty() {
                // A System One server has no listing route outside `/v1/models`
                // at all — `/api/*` requests must never reach one, so asking it
                // for `/api/tags` is both pointless and forbidden. Its models
                // are still routable and their names are known (declared in
                // `appconf.yaml`, kept in `available_models`), and the point of
                // the merged listing is that a client sees everything this proxy
                // can route to. Names only, and only for this family: a backend
                // of any other family that cannot serve this route contributes
                // nothing here, exactly as before.
                state_only.push((b.url.clone(), names));
            }
        }
    }
    if probed.is_empty() && state_only.is_empty() {
        return None;
    }

    let bodies = futures_util::future::join_all(probed.into_iter().map(|(base, token, names)| {
        let client = state.client.clone();
        let url = format!("{}{}", base, endpoint);
        async move {
            let body = async {
                let mut req = client.get(&url).timeout(crate::control::PROBE_TIMEOUT);
                if let Some(t) = &token {
                    req = req.header("Authorization", format!("Bearer {}", t));
                }
                let res = req.send().await.ok()?;
                if !res.status().is_success() {
                    return None;
                }
                res.json::<serde_json::Value>().await.ok()
            }
            .await;
            // The names travel with the answer so a backend that says
            // nothing still gets to contribute its own models.
            (body, names, base)
        }
    }))
    .await;

    // The first answering body, with the array key *it* reported its listing
    // under — on a route shared by two families that is not the key the merged
    // answer is written in.
    let mut envelope: Option<(serde_json::Value, &'static str)> = None;
    let mut merged: Vec<serde_json::Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut answered = 0usize;
    // Backends that gave no parseable listing, plus the names of backends that
    // were never asked at all: they are contributed in a second pass below,
    // once every live entry is in, so a bare synthesised entry can never take a
    // rich one's place whatever the backend order is.
    for (body, names, base) in bodies {
        // A backend that answers 200 with an error object (LM Studio does this
        // for foreign endpoints) has no listing to contribute. Likewise one
        // whose listing uses a shape this endpoint does not recognise, one that
        // answers the listing route with its own 404 and one that never
        // answers at all — all of them fall back to what we know about them.
        let shape = body.as_ref().and_then(|body| {
            shapes
                .iter()
                .copied()
                .find(|(key, _)| body.get(*key).is_some_and(|v| v.is_array()))
        });
        let (Some(body), Some((array_key, name_field))) = (body, shape) else {
            if !names.is_empty() {
                state_only.push((base, names));
            }
            continue;
        };
        let entries = body
            .get(array_key)
            .and_then(|m| m.as_array())
            .expect("array shape was just checked");
        // The merged list is always written in the route's own envelope, so an
        // entry read through the other one is re-keyed on the way in.
        for entry in entries {
            let name = entry.get(name_field).and_then(|v| v.as_str());
            if name.is_some_and(|id| !policy.served(id, &base)) {
                continue;
            }
            match name {
                // Already listed by an earlier backend.
                Some(id) if !seen.insert(id.to_string()) => continue,
                Some(_) => merged.push(retitled_entry(entry, name_field, out_field)),
                // Unnamed entry: pass it through rather than dropping it.
                None => merged.push(entry.clone()),
            }
        }
        answered += 1;
        if envelope.is_none() {
            envelope = Some((body, array_key));
        }
    }

    // The backends that could not list themselves, in backend order: one entry
    // per known name, written in this route's own envelope, skipping any name a
    // live entry already carried (first writer wins, and live writers went first).
    let mut from_state = 0usize;
    for (base, names) in state_only {
        for name in names {
            if !policy.served(&name, &base) || !seen.insert(name.clone()) {
                continue;
            }
            from_state += 1;
            let mut entry = serde_json::Map::new();
            entry.insert(out_field.to_string(), serde_json::Value::String(name));
            merged.push(serde_json::Value::Object(entry));
        }
    }

    // `array_key` is the key the first answering backend reported its listing
    // under — on a shared route that is not the key the answer uses, in which
    // case that array must not survive alongside the merged one. When nobody
    // answered but somebody has names, a fresh envelope is synthesised: the
    // names are routable and the client must learn them, rather than receiving
    // the backend's own 404 through the normal proxy path. `out_key` as the
    // consumed key makes the foreign-key removal below a no-op for it.
    let (mut envelope, consumed_key) = match envelope {
        Some(pair) => pair,
        None if merged.is_empty() => return None,
        None => (serde_json::Value::Object(serde_json::Map::new()), out_key),
    };
    debug!(
        "aggregated {} at {} model(s) from {} backend(s)",
        endpoint,
        merged.len(),
        answered
    );
    if from_state > 0 {
        debug!(
            "{} model(s) in the {} answer came from backend state: those backend(s) \
             do not answer their listing route",
            from_state, endpoint
        );
    }
    if consumed_key != out_key
        && let Some(map) = envelope.as_object_mut()
    {
        map.remove(consumed_key);
    }
    envelope[out_key] = serde_json::Value::Array(merged);
    Some((StatusCode::OK, axum::Json(envelope)).into_response())
}

/// Insert the backend's configured auth token into `headers` unless the
/// client already presented its own credentials (`Authorization` or
/// `X-Api-Key`). Client-supplied headers are never overridden; a token that
/// cannot be encoded as an HTTP header value is skipped rather than failing
/// the request.
fn attach_backend_token(headers: &mut HeaderMap, token: Option<&str>) {
    let t = match token {
        Some(t)
            if !headers.contains_key(axum::http::header::AUTHORIZATION)
                && !headers.contains_key("x-api-key") =>
        {
            t
        }
        _ => return,
    };
    if let Ok(v) = format!("Bearer {}", t).parse() {
        headers.insert(axum::http::header::AUTHORIZATION, v);
    }
}

/// True for endpoints that only read backend metadata — no inference.
///
/// These neither wait for a free slot nor occupy one: they don't touch the
/// model runner, so a `/api/tags` poll (the kind chat UIs issue on a timer)
/// must not queue behind a multi-minute generation under the default
/// `max_concurrent_per_backend: 1`, nor behind a model load/unload. Everything
/// else — generate, chat, embeddings, blob uploads, pulls — is scheduled
/// normally.
pub fn is_metadata_path(path: &str) -> bool {
    matches!(
        path,
        "/" | "/api/tags" | "/api/ps" | "/api/version" | "/api/show" | "/v1/models"
    ) || path.starts_with("/v1/models/")
}

/// One entry from LM Studio's native model list (`GET /api/v1/models`):
/// the model key, optional display name, and ids of currently loaded instances
/// (used by model control to resolve the `instance_id` for unloading).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LmModelInfo {
    pub key: String,
    pub display_name: Option<String>,
    pub loaded_instance_ids: Vec<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct BackendStatus {
    pub url: String,
    /// Optional API key for this backend (from appconf.yaml), sent as
    /// `Authorization: Bearer <token>` on every request the proxy makes to
    /// it — proxied inference, metadata reads, health probes and model
    /// control ops. A client's own auth header is never overridden.
    pub token: Option<String>,
    pub active_requests: usize,
    pub processed_count: usize,
    pub is_online: bool,
    pub api_type: BackendApiType,
    /// Reference-counted: replaced wholesale by each probe, but cloned by
    /// the TUI on every frame (a backend can list hundreds of models).
    pub available_models: Arc<HashSet<String>>,
    pub loaded_models: Arc<HashSet<String>>,
    /// True when the backend reported loaded-model data in its latest probe
    /// (Ollama `/api/ps` or LM Studio `/api/v1/models`). `false` means the
    /// loaded state is UNKNOWN (no such endpoint, or it is skipped as bad) —
    /// for such backends (e.g. vLLM) "available" implies "ready to serve".
    pub loaded_state_known: bool,
    /// Actual context window of each resident model, keyed by model name/key
    /// (Ollama `/api/ps` `context_length`; LM Studio loaded instance
    /// `config.context_length`). Used to detect resident models whose
    /// context doesn't match the configured `max_ctx`.
    pub loaded_ctx: HashMap<String, u64>,
    pub current_model: Option<String>,
    /// In-flight request count keyed by `model_concurrency_key`. Only
    /// populated for requests that carry a model; enforces the per-model
    /// `max_concurrent_requests` limits from appconf.yaml (Plan D).
    pub active_by_model: HashMap<String, u32>,
    /// True when the backend speaks the LM Studio native REST API
    /// (`/api/v1/models*`), which enables model load/unload control.
    pub lmstudio: bool,
    /// LM Studio native model list (empty for other backend types).
    /// Reference-counted for the same reason as `available_models`.
    pub native_models: Arc<Vec<LmModelInfo>>,
    /// Endpoints this backend is known to reject (remembered from probes);
    /// the health loop skips them until a full re-probe.
    pub known_bad_endpoints: HashSet<String>,
    /// API families learned (from real traffic) to be rejected by this
    /// backend, e.g. Ollama-family requests answered with an endpoint error
    /// by an LM Studio server. The scheduler excludes such backends for that
    /// family until a request of the family succeeds or the backend restarts.
    pub rejected_families: HashSet<ApiFamily>,
    /// Consecutive endpoint-rejection responses per API family; at 2 the
    /// family moves into `rejected_families`. Cleared on any success.
    pub family_fail_counts: HashMap<ApiFamily, u8>,
    /// API family pinned by this backend's `kind:` config key, empty when the
    /// family is to be learned from probes (see [`crate::config::Backend::kind`]).
    /// Configuration, not an observation: `apply_probe` keeps this value instead
    /// of the probe's, because a Laya-style backend answers nothing that
    /// identifies it and would otherwise be classified `???` forever.
    pub forced_api: ApiSet,
    /// Model names declared for this backend in `appconf.yaml` `models:`,
    /// resolved with the same selector that decides where models get loaded
    /// (`control::config_targets`). Admitted into `available_models` by
    /// `apply_probe` alongside whatever the probe discovered, because a name the
    /// operator wrote down is a name they will send requests for — and
    /// `model_routable` rejects any backend that does not list it.
    ///
    /// Only meaningful for backends that cannot list their own models (Laya has
    /// no listing endpoint); for backends that can, the union is a no-op in
    /// practice because the declared names are also the discovered ones.
    pub declared_models: Vec<String>,
    /// Parallel request slots the server reports (see `BackendProbe::slots`).
    pub detected_slots: Option<u32>,
    /// Percent of the KV cache free (0–100), when the server reports it (see
    /// `BackendProbe::kv_free_pct`). Sizes the free capacity on the Overview.
    pub kv_free_pct: Option<u8>,
    /// Display name (`backends[].name`), shown instead of `host:port`.
    pub name: Option<String>,
    /// Turned off in the settings: never probed, kept offline, never routed to.
    pub disabled: bool,
}

impl BackendStatus {
    /// A backend nothing is known about yet; the health loop fills it in.
    pub fn new(url: String, token: Option<String>, declared_models: Vec<String>) -> Self {
        BackendStatus {
            url,
            token,
            active_requests: 0,
            processed_count: 0,
            is_online: true,
            api_type: BackendApiType::Unknown,
            available_models: Arc::default(),
            loaded_models: Arc::default(),
            loaded_state_known: false,
            loaded_ctx: HashMap::new(),
            current_model: None,
            active_by_model: HashMap::new(),
            lmstudio: false,
            native_models: Arc::default(),
            known_bad_endpoints: HashSet::new(),
            rejected_families: HashSet::new(),
            family_fail_counts: HashMap::new(),
            forced_api: ApiSet::EMPTY,
            declared_models,
            detected_slots: None,
            kv_free_pct: None,
            name: None,
            disabled: false,
        }
    }
}

pub struct AppState {
    pub queues: Mutex<HashMap<String, VecDeque<Task>>>,
    pub processing_counts: Mutex<HashMap<String, usize>>,
    pub processed_counts: Mutex<HashMap<String, usize>>,
    pub dropped_counts: Mutex<HashMap<String, usize>>,
    pub user_ips: Mutex<HashMap<String, IpAddr>>,
    pub blocked_ips: Mutex<HashSet<IpAddr>>,
    pub blocked_users: Mutex<HashSet<String>>,
    pub vip_user: Mutex<Option<String>>,
    pub boost_user: Mutex<Option<String>>,
    /// Round counter driving the boost user's every-other-turn slot. Atomic
    /// rather than a `Mutex` so it is not part of the lock stack the scheduler
    /// already holds while choosing a task.
    pub global_counter: std::sync::atomic::AtomicUsize,
    /// Fair-share scheduling scores, keyed by user. See [`FairShare`].
    pub fair_share: Mutex<HashMap<String, FairShare>>,
    /// Which backend each conversation was last served by, for sticky routing
    /// (see [`Affinity`] and `settings.sticky_routing`).
    pub affinity: Mutex<HashMap<SessionKey, Affinity>>,
    /// Last measured prefix-cache hit of each conversation on each backend,
    /// keyed by (session, backend url). See [`ConvCache`] and [`cold_hold`].
    pub conv_cache: Mutex<HashMap<(SessionKey, String), ConvCache>>,
    /// When each session last finished a turn — the slot-reservation window
    /// for its follow-up request. See [`SLOT_RESERVE`].
    pub slot_reserve: Mutex<HashMap<SessionKey, SlotReserve>>,
    /// `settings.sticky_routing` as a [`StickyMode`] disc; atomic so the
    /// scheduler reads it without another lock.
    pub sticky_mode: std::sync::atomic::AtomicU8,
    /// `settings.sticky_ttl` (seconds).
    pub sticky_ttl: std::sync::atomic::AtomicU64,
    /// `settings.sticky_lag`.
    pub sticky_lag: std::sync::atomic::AtomicU64,
    /// `settings.sticky_max_entries`.
    pub sticky_max_entries: std::sync::atomic::AtomicU64,
    pub notify: Notify,
    pub backend_freed: Notify,
    pub backends: Mutex<Vec<BackendStatus>>,
    /// Last backend index handed a request, for round-robin among equally
    /// loaded backends. Atomic for the same reason as `global_counter`.
    pub last_backend_idx: std::sync::atomic::AtomicUsize,
    /// Request timeout in seconds (`settings.timeout`), live-editable.
    pub timeout: std::sync::atomic::AtomicU64,
    /// How long a request may wait when no backend can *ever* serve it
    /// (all offline / wrong API family / model absent everywhere) before the
    /// proxy answers 503 instead of letting the client hang. Requests that
    /// are merely waiting for a busy or loading backend are exempt.
    pub stuck_timeout_secs: std::sync::atomic::AtomicU64,
    /// Shared HTTP client for proxying and backend control probes.
    pub client: reqwest::Client,
    /// In-flight model control operations, keyed by backend index
    /// (at most one per backend). Backends with an entry are treated as busy
    /// by the scheduler.
    pub control_ops: Mutex<HashMap<usize, crate::control::ControlOp>>,
    /// Recent finished control operations (bounded), for TUI feedback.
    pub control_history: Mutex<VecDeque<crate::control::ControlResult>>,
    /// `keep_alive` (seconds) sent with control "load" requests so explicitly
    /// loaded models stay resident (Ollama's own default is only 5 minutes).
    pub load_keep_alive: std::sync::atomic::AtomicI64,
    /// Max simultaneous in-flight requests per backend (default 1 — the
    /// historical behavior). Per-model sub-limits come from `model_limits`.
    pub max_concurrent_per_backend: std::sync::atomic::AtomicU32,
    /// Use the slot count a backend reports as its capacity
    /// (`settings.detect_backend_slots`, default on).
    pub detect_slots: std::sync::atomic::AtomicBool,
    /// Model aliases, keyed by lowercase alias (`settings.model_aliases`).
    pub model_aliases: Mutex<HashMap<String, String>>,
    /// Keyless requests without `X-User-ID` queue as `ip-<address>`
    /// (`settings.ip_identity`, default on).
    pub ip_identity: std::sync::atomic::AtomicBool,
    /// Per-model concurrency limits keyed by `model_concurrency_key`; rebuilt
    /// whenever the model config changes. Unlisted models default to 1.
    pub model_limits: Mutex<HashMap<String, u32>>,
    /// Model configuration from `appconf.yaml` (models to load on backends,
    /// with load settings). Re-read with the TUI 'r' key.
    pub model_config: Mutex<Vec<crate::config::ModelConfig>>,
    /// Path of the model config file.
    pub model_config_path: String,
    /// Ring buffer of recent request/control events for the TUI Logs panel.
    pub logs: Mutex<VecDeque<LogEvent>>,
    /// Size-rotated request/response content logger (no-op when disabled).
    pub reqlog: crate::reqlog::RequestLogger,
    /// Max bytes of body content captured per record in the request log.
    pub log_content_limit: std::sync::atomic::AtomicUsize,
    /// Ceiling on `queued_bytes` (`settings.max_queued_bytes`).
    pub max_queued_bytes: std::sync::atomic::AtomicU64,
    /// Proxy routes the proxy does not know (`settings.allow_all_routes`).
    pub allow_all_routes: std::sync::atomic::AtomicBool,
    /// Bearer token `GET /metrics` requires; `None` leaves it open.
    pub metrics_token: Mutex<Option<String>>,
    /// Where each setting comes from: appconf.yaml, the web UI's overlay,
    /// and the CLI/env values that pin some of them.
    pub config: Mutex<crate::settings::Layers>,
    /// Bytes of request bodies currently waiting in the queues. Queued tasks
    /// own their whole body, so the per-user request cap alone still allowed
    /// gigabytes to pile up; this bounds it in bytes. Read and updated only
    /// under the `queues` lock, so the checks are atomic with the push/pop.
    pub queued_bytes: std::sync::atomic::AtomicU64,
    /// Clients and their API keys (see [`crate::clients`]). Starts empty and
    /// in memory; `main` swaps in the store loaded from `clients_file`.
    pub clients: Mutex<crate::clients::ClientStore>,
    /// Which models are served (see [`crate::model_policy`]). Starts open and
    /// in memory; `main` swaps in the store loaded from `models_file`. A leaf
    /// lock: nothing is acquired while it is held.
    pub model_policy: Mutex<crate::model_policy::ModelPolicyStore>,
    /// Prometheus counters and histograms (see [`crate::metrics`]).
    pub metrics: crate::metrics::Metrics,
    /// Requests being served right now (live views).
    pub runs: crate::metrics::Runs,
    /// The last hour of finished requests and activity, for the Overview.
    pub stats: crate::stats::Stats,
    /// Events evicted from the front of `logs` so far. The event at index `i`
    /// of the ring has the stable sequence number `logs_evicted + i`, which
    /// the web UI uses to fetch one event's full content.
    pub logs_evicted: std::sync::atomic::AtomicU64,
}

/// One line of the TUI Logs panel: a request entering ("IN") or leaving
/// ("OUT") the proxy, or a model-control action ("CTL").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEvent {
    pub at: std::time::SystemTime,
    /// "IN" | "OUT" | "CTL"
    pub dir: &'static str,
    pub user: String,
    pub model: Option<String>,
    pub backend: Option<String>,
    /// Status code or short note ("queued", "dropped", …).
    pub info: String,
    /// Optional body/response content preview (truncated to the configured
    /// limit) for IN/OUT events; None for drops and control actions.
    /// Reference-counted: the TUI snapshots the newest events on every frame,
    /// and these strings run to `log_content_limit` (64 KiB by default).
    pub content: Option<Arc<String>>,
}

/// Half-life of a user's fair-share score: load older than this counts half
/// as much when deciding whose turn it is.
const FAIR_SHARE_HALF_LIFE: std::time::Duration = std::time::Duration::from_secs(300);

/// Recent scheduling load for one user, as an exponentially decaying score.
///
/// Ordering used to be by `processed_counts`, which is cumulative since
/// startup and only counts fully delivered responses. Two things went wrong
/// with that: a client whose requests always failed or disconnected never
/// incremented it and so kept permanent priority over everyone else, and on a
/// long-lived proxy a newcomer started thousands of requests "behind" an
/// incumbent and monopolized every backend until it caught up. This score is
/// charged when a request is DISPATCHED — whatever the outcome — and halves
/// every [`FAIR_SHARE_HALF_LIFE`], so the order reflects recent load only.
#[derive(Clone, Copy, Debug)]
pub struct FairShare {
    score: f64,
    /// Both the decay reference point and the user's last-seen time.
    updated: std::time::Instant,
}

impl FairShare {
    fn new(now: std::time::Instant) -> Self {
        Self {
            score: 0.0,
            updated: now,
        }
    }

    /// The score as of `now`, after decay. Cheap and side-effect free.
    pub(crate) fn value_at(&self, now: std::time::Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.score * 0.5f64.powf(elapsed / FAIR_SHARE_HALF_LIFE.as_secs_f64())
    }

    /// Roll the decay forward to `now` without adding load — marks the user
    /// as seen while keeping the decay maths exact.
    fn touch(&mut self, now: std::time::Instant) {
        self.score = self.value_at(now);
        self.updated = now;
    }

    /// Roll forward and charge one dispatched request.
    fn charge(&mut self, now: std::time::Instant) {
        self.touch(now);
        self.score += 1.0;
    }

    /// When this user was last seen (request arrived or was dispatched).
    fn last_seen(&self) -> std::time::Instant {
        self.updated
    }
}

/// What identifies a conversation for sticky routing. A model is part of the
/// key because a different model is a different KV cache: moving between models
/// is not "staying put".
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub conversation: String,
    pub model: String,
}

impl SessionKey {
    /// Stable short tag for the UI's session views: six hex digits of the
    /// conversation. Collision across sessions at 16M in 24 bits is ~0%
    /// for any realistic number of live conversations.
    pub fn tag(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.conversation.hash(&mut h);
        format!("{:06x}", h.finish() & 0x00ff_ffff)
    }
}

/// The backend a conversation was last served by.
#[derive(Clone, Debug)]
pub struct Affinity {
    pub backend_url: String,
    last_use: std::time::Instant,
}

/// A conversation the backend served this many times more recently than its
/// last own service is treated as cache-resident and never held back: those
/// are the turns the cache still holds.
const CACHE_GRACE: usize = 2;

/// A cache-cold front request yields at most this many cache-hot dispatches
/// before it goes regardless — the fairness bound on [`cold_hold`].
const CACHE_HOLD_PASSES: u8 = 16;

/// The last measured prefill hit at or above this keeps a conversation in
/// front: its prefix is resident and its next turn is nearly free.
const CACHE_HOT_HIT_PCT: u8 = 50;

/// The longest a measured cache hit is trusted. Older than this the conversation
/// is simply unknown (an idle eviction may have flushed it), and unknown is
/// never treated as cold.
const CACHE_STATE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// How long a conversation's row (hit measurement + turn history) is kept
/// before prune drops it. Longer than [`CACHE_STATE_TTL`] on purpose: under
/// saturation a conversation may only get served every few minutes, and the
/// turn count — the evidence that a miss is cache contention rather than a
/// first turn — must survive that.
const CACHE_HISTORY_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(4 * 3600);

/// How long a session's follow-up request outranks every other user's queue
/// when nothing is measured about that session yet: the client's local tool
/// time between receiving an answer and sending the next request. Measured on
/// the reference deployment's request log — median 0.5 s, p99 4.1 s, max 4.3 s
/// over 595 turn-to-turn continuations. See [`reserve_window`] for how the
/// window adapts to each session. It costs nothing when the session does not
/// come back (no queued task, no reorder) and at most one turn per bystander
/// when it does.
const SLOT_RESERVE: std::time::Duration = std::time::Duration::from_secs(5);

/// Bounds on the adaptive reservation window (see [`reserve_window`]).
const SLOT_RESERVE_MIN: std::time::Duration = std::time::Duration::from_secs(2);
const SLOT_RESERVE_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// How many consecutive dispatches a session may win on its reservation
/// before it must stand in the ordinary fair order for one round. Without it
/// a never-quiet agent keeps the slot indefinitely — fine for its cache, not
/// for the queue behind it.
const SLOT_RESERVE_CHAIN: u16 = 64;

/// A follow-up observed this far after the previous completion is a new
/// engagement, not a tool turnaround: it never feeds the gap average.
const SLOT_GAP_IGNORE: std::time::Duration = std::time::Duration::from_secs(900);

/// What the reservation knows about one session: when its last turn
/// completed, the session's recent worst turn-to-turn turnaround (ms, decay
/// 2% per later observation; 0 = nothing measured yet), and how many reserved
/// dispatches it has won in a row.
#[derive(Clone, Copy, Debug)]
pub struct SlotReserve {
    at: std::time::Instant,
    gap_max_ms: f64,
    chain: u16,
}

/// The reservation window for a session: 1.5x its recent worst turnaround —
/// a session keeps its slot at least as long as it ever hesitated, so a fleet
/// with slow tools (sandbox spins, package installs) is not scheduled by one
/// deployment's median — clamped to [`SLOT_RESERVE_MIN`]..=[`SLOT_RESERVE_MAX`].
fn reserve_window(gap_max_ms: f64) -> std::time::Duration {
    if gap_max_ms <= 0.0 {
        return SLOT_RESERVE;
    }
    let ms = (1.5 * gap_max_ms).clamp(
        SLOT_RESERVE_MIN.as_secs_f64() * 1000.0,
        SLOT_RESERVE_MAX.as_secs_f64() * 1000.0,
    );
    std::time::Duration::from_secs_f64(ms / 1000.0)
}

/// The last measured prefix-cache hit of one conversation on one backend,
/// taken from the backend's own usage stats. This is what tells the scheduler
/// whose context is currently resident where.
#[derive(Clone, Copy, Debug)]
pub struct ConvCache {
    hit_pct: u8,
    at: std::time::Instant,
    /// Completed turns of this conversation on this backend. A first-turn
    /// miss is not evidence of cache contention — the context was simply
    /// never there — so it never holds.
    turns: u32,
    /// Tokens the backend evaluated last turn plus what it decoded since —
    /// the estimated incremental prefill work of the next turn if the cache
    /// kept everything else. `None` for servers that report no `prompt_eval`.
    inc_tok: Option<u64>,
}

/// The share of a prompt the backend served from its prefix cache, percent
/// 0–100 — the same accounting as the activity view's `cache %` column.
/// `None` when the server reported no prompt counts at all.
fn cache_hit_pct(prompt: u64, evaluated: Option<u64>) -> Option<u8> {
    let evaluated = evaluated?;
    if prompt == 0 {
        return None;
    }
    Some(100u8.saturating_sub((evaluated.min(prompt) * 100 / prompt) as u8))
}

/// Remember how much of this conversation's prompt the backend skipped.
fn record_conv_cache(
    state: &AppState,
    key: &SessionKey,
    backend_url: &str,
    hit_pct: u8,
    inc_tok: Option<u64>,
) {
    let mut table = state.conv_cache.lock_or_recover();
    let entry = table.entry((key.clone(), backend_url.to_string())).or_insert(ConvCache {
        hit_pct: 0,
        at: std::time::Instant::now(),
        turns: 0,
        inc_tok: None,
    });
    entry.turns = entry.turns.saturating_add(1);
    entry.hit_pct = hit_pct;
    entry.inc_tok = inc_tok;
    entry.at = std::time::Instant::now();
}

/// Note a finished turn so the session's follow-up can keep priority for
/// [`SLOT_RESERVE`] (see [`slot_reserved`]).
fn reserve_slot(state: &AppState, key: &SessionKey) {
    let mut table = state.slot_reserve.lock_or_recover();
    let entry = table.entry(key.clone()).or_insert(SlotReserve {
        at: std::time::Instant::now(),
        gap_max_ms: 0.0,
        chain: 0,
    });
    entry.at = std::time::Instant::now();
}

/// Feed the observed client turnaround of a dispatching follow-up into the
/// session's reservation: `arrived` is when the request entered the queue, so
/// the gap since the last completion is exactly the tool time the reservation
/// exists to cover. Also tracks the chain of consecutive reserved wins: a win
/// at the cap resets it, forcing exactly one ordinary fair round every
/// [`SLOT_RESERVE_CHAIN`] turns, and any dispatch that came in outside the
/// window resets it too.
fn note_follow_up(state: &AppState, key: &SessionKey, arrived: std::time::Instant) {
    let mut table = state.slot_reserve.lock_or_recover();
    let Some(entry) = table.get_mut(key) else {
        return;
    };
    let Some(gap) = arrived.checked_duration_since(entry.at) else {
        entry.chain = 0; // queued during its own previous turn: not a turnaround
        return;
    };
    if gap >= SLOT_GAP_IGNORE {
        entry.chain = 0; // a new engagement, not a running cycle
        return;
    }
    let gap_ms = gap.as_secs_f64() * 1000.0;
    entry.chain = if gap < reserve_window(entry.gap_max_ms) {
        if entry.chain >= SLOT_RESERVE_CHAIN {
            0
        } else {
            entry.chain.saturating_add(1)
        }
    } else {
        0
    };
    // Decaying max: the window covers this session's worst recent pause, and
    // a fleet-wide slowdown is followed within a few turns.
    entry.gap_max_ms = gap_ms.max(entry.gap_max_ms * 0.98);
}

/// Decide whether a user's front request should wait this selection round.
/// Its conversation last MISSED the prefix cache on the backend that last
/// served it, that backend reports its KV cache (halogen with metrics — a
/// plain Ollama reports nothing and is never held) and the conversation's
/// estimated next-turn increment does not fit the free KV (or the backend
/// cannot size it and the cache is simply not empty), more recently served
/// conversations may still own that cache (the grace window), and the request
/// has not yet spent [`CACHE_HOLD_PASSES`] yields.
///
/// Why a yield budget instead of a wait bound: under a saturated backlog every
/// request has already waited longer than any sane time valve, so a time bound
/// disarms exactly when the hold-back matters; a per-request yield budget
/// bounds the hold itself whatever the queue depth.
///
/// Every unknown counts as not cold: no session identity, no fresh
/// measurement, or a backend that reports no KV usage.
fn cold_hold(
    session_key: Option<&SessionKey>,
    passes: u8,
    affinity: &HashMap<SessionKey, Affinity>,
    conv_cache: &HashMap<(SessionKey, String), ConvCache>,
    backends: &[BackendStatus],
    now: std::time::Instant,
) -> bool {
    if passes >= CACHE_HOLD_PASSES {
        return false;
    }
    let Some(key) = session_key else {
        return false;
    };
    let Some(aff) = affinity.get(key) else {
        return false;
    };
    let Some(entry) = conv_cache.get(&(key.clone(), aff.backend_url.clone())) else {
        return false;
    };
    if now.saturating_duration_since(entry.at) >= CACHE_STATE_TTL {
        return false;
    }
    if entry.hit_pct >= CACHE_HOT_HIT_PCT || entry.turns < 2 {
        return false;
    }
    // The conversations served most recently still own the cache: their turns
    // are the cheap ones; this one would only evict them.
    let fresher = conv_cache
        .iter()
        .filter(|((k, url), c)| {
            url == &aff.backend_url
                && k != key
                && now.saturating_duration_since(c.at) < CACHE_STATE_TTL
                && c.at > entry.at
        })
        .count();
    if fresher < CACHE_GRACE {
        return false;
    }
    // The budget rule, when both sides are measured: the next turn's
    // incremental prefill (what the backend evaluated and decoded last turn)
    // against the free KV it reports. A conversation whose increment does not
    // fit evicts the resident set however it is ordered: send it to the back.
    // When either side is unmeasured (no `prompt_eval`, no reported context
    // size) fall back to the pressure signal alone: a not-empty cache means
    // somebody could be evicted.
    let Some(b) = backends.iter().find(|b| b.url == aff.backend_url) else {
        return false;
    };
    // shortcut: sums resident model contexts as the KV pool; if a multi-model
    // backend turns out to keep per-model pools, size it from that instead.
    match (entry.inc_tok, b.kv_free_pct, b.loaded_ctx.values().sum::<u64>()) {
        (Some(inc), Some(free_pct), total) if total > 0 && free_pct < 100 => {
            inc > total.saturating_mul(free_pct as u64) / 100
        }
        (_, Some(free_pct), _) => free_pct < 100,
        _ => false,
    }
}

/// Whether this session finished a turn recently enough that its queued
/// follow-up goes first this round: inside its adaptive reservation window
/// (see [`reserve_window`]) and under the consecutive-win cap
/// ([`SLOT_RESERVE_CHAIN`]). Fail-open: an unknown session or no reservation
/// is never privileged.
fn slot_reserved(
    session_key: Option<&SessionKey>,
    reserved: &HashMap<SessionKey, SlotReserve>,
    now: std::time::Instant,
) -> bool {
    session_key.is_some_and(|key| {
        reserved.get(key).is_some_and(|e| {
            e.chain < SLOT_RESERVE_CHAIN
                && now.saturating_duration_since(e.at) < reserve_window(e.gap_max_ms)
        })
    })
}

/// Bytes of a prompt's canonical form fed into the fingerprint. The leading
/// turns are what identifies a thread, so a bounded prefix is enough and a
/// 200k-token body never has to be hashed whole.
const FINGERPRINT_BYTES: usize = 16 * 1024;

/// True for routes that carry a conversation, the only ones worth fingerprinting.
pub fn is_conversation_path(path: &str) -> bool {
    matches!(
        path,
        "/api/chat" | "/api/generate" | "/v1/chat/completions" | "/v1/completions"
    )
}

/// The sticky-routing key for a request, or `None` for no affinity.
///
/// Explicit session ids always win when present, whatever the mode: a client
/// that tells us which thread a request belongs to is more reliable than any
/// guess we can make from the prompt. Otherwise `prompt` mode fingerprints the
/// conversation (chat clients resend the whole history every turn, so two turns
/// of one thread share a prefix) and `identity` mode falls back to the client
/// id, which is right for one-session-per-user setups and wrong for a client
/// running several conversations at once.
///
/// Only completion routes are keyed; a metadata read or a pull has no cache to
/// stay near.
pub fn derive_session_key(
    mode: StickyMode,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
    model: Option<&str>,
    user: &str,
) -> Option<SessionKey> {
    if mode == StickyMode::Off || !is_conversation_path(path) {
        return None;
    }
    let conversation = session_id_header(headers).or_else(|| match mode {
        // One pass over the body: an explicit `prompt_cache_key` if the client
        // sent one, else the fingerprint of the prompt.
        StickyMode::Prompt => {
            let fp = parse_prompt(body)?;
            fp.prompt_cache_key
                .clone()
                .or_else(|| prompt_fingerprint(&fp).map(|fp| format!("p:{fp:016x}")))
        }
        StickyMode::Header => body_cache_key(body),
        StickyMode::Identity => Some(format!("u:{user}")),
        StickyMode::Off => None,
    })?;
    Some(SessionKey {
        conversation,
        model: model.unwrap_or("-").to_string(),
    })
}

/// An explicit conversation id: the `X-Session-Id` request header, or
/// OpenAI / Ollama's own `prompt_cache_key` body field.
fn session_id_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Run `get` over a JSON object body without buffering fields it never asks
/// for, mirroring [`extract_requested_model`]'s no-parse-into-Value approach.
/// Non-object bodies (and any parse error) yield `None`.
fn json_object_get<T>(body: &[u8], get: impl FnOnce(T) -> Option<String>) -> Option<String>
where
    T: serde::de::DeserializeOwned,
{
    if body
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_none_or(|b| *b != b'{')
    {
        return None;
    }
    serde_json::from_slice::<T>(body).ok().and_then(get)
}

/// A client-supplied `prompt_cache_key` in the body, if it has one.
fn body_cache_key(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct CacheKey {
        #[serde(default)]
        prompt_cache_key: Option<String>,
    }
    json_object_get(body, |v: CacheKey| v.prompt_cache_key)
}

/// The conversation-defining fields of a completion body.
#[derive(Deserialize)]
struct Fp {
    #[serde(default)]
    prompt_cache_key: Option<String>,
    #[serde(default)]
    prompt: Option<serde_json::Value>,
    #[serde(default)]
    messages: Option<Vec<serde_json::Value>>,
}

/// The body as an [`Fp`], or `None` for a body that is not a JSON object.
fn parse_prompt(body: &[u8]) -> Option<Fp> {
    // The visitor pulls just the conversation fields out of the object and
    // ignores the rest (sampling params, tools, the whole tool schema ...).
    struct FpVisitor;
    impl<'de> serde::de::Visitor<'de> for FpVisitor {
        type Value = Fp;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a JSON object")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Fp, A::Error> {
            let mut prompt = None;
            let mut messages = None;
            let mut prompt_cache_key = None;
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "prompt" => prompt = Some(map.next_value()?),
                    "messages" => messages = Some(map.next_value()?),
                    "prompt_cache_key" => prompt_cache_key = Some(map.next_value()?),
                    _ => {
                        let _ = map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
            }
            Ok(Fp {
                prompt_cache_key,
                prompt,
                messages,
            })
        }
    }
    <serde_json::Deserializer<serde_json::de::SliceRead>>::from_slice(body)
        .deserialize_any(FpVisitor)
        .ok()
}

/// A stable hash of what identifies a conversation: its first non-system turn.
///
/// That turn is the one part of the request that never changes — every later turn
/// appends to the history and rewrites everything after it, so anything derived
/// from the tail (or from the whole history) gives a different key each turn and
/// the conversation is treated as new on every request. Hashing the opening turn
/// instead keeps one thread on one backend for its whole life, whatever its
/// length. Canonicalised (map keys in sorted order) so whitespace and field order
/// do not change it, capped at [`FINGERPRINT_BYTES`], and a leading `system` turn
/// is skipped so the identity comes from the conversation, not the app's
/// boilerplate. A parse problem means no fingerprint, never a wrong one.
///
/// Two threads that open with the same words (say "hello") collide onto one
/// preference. That is harmless: the key is per client and per model, and a
/// preference the scheduler may still override, so the worst case is that one
/// client's conversations queue together — which is what they do anyway.
///
/// Cost: one more pass over an already-buffered body per completion request, with
/// the conversation fields held as a tree while they are hashed. That is
/// deliberate: a prefix of the raw bytes would break on whitespace and field
/// order, which is exactly what the fingerprint has to be blind to.
/// shortcut: those fields are parsed whole even though only the opening turn is
/// hashed; replace the parse with a streaming visitor that stops at the first
/// message if chat bodies routinely reach tens of MB.
fn prompt_fingerprint(fp: &Fp) -> Option<u64> {
    let mut h = BudgetHasher::default();
    match (&fp.prompt, &fp.messages) {
        (_, Some(msgs)) => {
            let opening = msgs
                .iter()
                .find(|m| m.get("role").and_then(|r| r.as_str()) != Some("system"))?;
            write_canonical(opening, &mut h);
        }
        (Some(serde_json::Value::String(s)), None) => {
            h.write(&s.as_bytes()[..s.len().min(FINGERPRINT_BYTES)]);
        }
        (Some(v), None) => write_canonical(v, &mut h),
        (None, None) => return None,
    }
    (h.bytes > 0).then(|| h.finish())
}

/// A hasher that counts the bytes fed to it, so the fingerprint can stop early.
#[derive(Default)]
struct BudgetHasher {
    inner: std::collections::hash_map::DefaultHasher,
    bytes: usize,
}

impl std::hash::Hasher for BudgetHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.bytes += bytes.len();
        self.inner.write(bytes);
    }
    fn finish(&self) -> u64 {
        self.inner.finish()
    }
}

/// Hash the canonical form of one JSON value: object keys in sorted order, so
/// two serializations of the same content hash the same.
fn write_canonical(v: &serde_json::Value, h: &mut BudgetHasher) {
    use std::hash::Hash;
    if h.bytes >= FINGERPRINT_BYTES {
        return;
    }
    match v {
        serde_json::Value::Null => h.write(b"n"),
        serde_json::Value::Bool(b) => b.hash(h),
        serde_json::Value::Number(n) => n.to_string().hash(h),
        serde_json::Value::String(s) => {
            h.write(s.as_bytes());
        }
        serde_json::Value::Array(xs) => {
            h.write_u8(b'[');
            for x in xs {
                write_canonical(x, h);
            }
            h.write_u8(b']');
        }
        serde_json::Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            h.write_u8(b'{');
            for k in keys {
                h.write(k.as_bytes());
                h.write_u8(b'=');
                write_canonical(&m[k], h);
            }
            h.write_u8(b'}');
        }
    }
}

/// The backend a conversation should prefer, if any, chosen by the scheduler
/// BEFORE the usual least-connections pick.
///
/// A preference, never a pin: if the remembered backend cannot take the request
/// now, or is running more than `sticky_lag` more than the lightest eligible
/// backend, the conversation moves and the mapping follows. A hard pin would
/// stall the user's own queue — the scheduler takes the first ROUTABLE task per
/// user, so tasks pinned to a busy backend all sit unroutable while other
/// backends idle. Moving costs one re-prefill; stalling costs everything behind
/// it in that queue.
fn pick_sticky_backend(
    state: &AppState,
    key: &SessionKey,
    eligible: &[usize],
    backends: &[BackendStatus],
    min_conns: usize,
) -> Option<usize> {
    let ttl = std::time::Duration::from_secs(
        state
            .sticky_ttl
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    let lag = state
        .sticky_lag
        .load(std::sync::atomic::Ordering::Relaxed);
    let now = std::time::Instant::now();
    let mut table = state.affinity.lock_or_recover();
    let idx = table.get(key).and_then(|a| {
        // Fresh = remembered no longer ago than the TTL.
        if now.saturating_duration_since(a.last_use) > ttl {
            return None;
        }
        eligible
            .iter()
            .copied()
            .find(|&i| backends[i].url == a.backend_url)
    })?;
    if backends[idx].active_requests > min_conns + lag as usize {
        state
            .metrics
            .inc(&crate::metrics::AFFINITY_DECISIONS, &["moved"], 1.0);
        return None;
    }
    table.get_mut(key).unwrap().last_use = now;
    state
        .metrics
        .inc(&crate::metrics::AFFINITY_DECISIONS, &["hit"], 1.0);
    Some(idx)
}

/// Remember where a conversation went, so its next turn can come back here.
fn record_affinity(state: &AppState, key: &SessionKey, backend_url: &str) {
    let mut table = state.affinity.lock_or_recover();
    match table.get_mut(key) {
        Some(a) => {
            a.backend_url = backend_url.to_string();
            a.last_use = std::time::Instant::now();
        }
        None => {
            table.insert(
                key.clone(),
                Affinity {
                    backend_url: backend_url.to_string(),
                    last_use: std::time::Instant::now(),
                },
            );
        }
    }
}

/// Drop conversations idle for longer than `sticky_ttl`, then the idlest past
/// `sticky_max_entries`, plus anything pointing at a backend the proxy no longer
/// has. Runs from the same loop that prunes idle users.
fn prune_affinity(state: &AppState) {
    let ttl = std::time::Duration::from_secs(
        state
            .sticky_ttl
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    let max = state
        .sticky_max_entries
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let now = std::time::Instant::now();
    let urls: HashSet<String> = state
        .backends
        .lock_or_recover()
        .iter()
        .map(|b| b.url.clone())
        .collect();
    let mut table = state.affinity.lock_or_recover();
    table.retain(|_, a| {
        now.saturating_duration_since(a.last_use) < ttl && urls.contains(&a.backend_url)
    });
    if table.len() > max {
        let mut idle: Vec<(SessionKey, std::time::Instant)> = table
            .iter()
            .map(|(k, a)| (k.clone(), a.last_use))
            .collect();
        idle.sort_by_key(|(_, seen)| *seen);
        for (k, _) in idle.into_iter().take(table.len() - max) {
            table.remove(&k);
        }
    }
}

/// Forget cache measurements idle for [`CACHE_HISTORY_MAX_AGE`], the idlest past
/// `sticky_max_entries` (the table tracks the same live conversations as the
/// affinity table does), and anything pointing at a removed backend.
fn prune_conv_cache(state: &AppState) {
    let max = state
        .sticky_max_entries
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let now = std::time::Instant::now();
    let urls: HashSet<String> = state
        .backends
        .lock_or_recover()
        .iter()
        .map(|b| b.url.clone())
        .collect();
    let mut table = state.conv_cache.lock_or_recover();
    table.retain(|(_, url), c| {
        now.saturating_duration_since(c.at) < CACHE_HISTORY_MAX_AGE && urls.contains(url)
    });
    if table.len() > max {
        let mut idle: Vec<((SessionKey, String), std::time::Instant)> = table
            .iter()
            .map(|(k, c)| (k.clone(), c.at))
            .collect();
        idle.sort_by_key(|(_, at)| *at);
        for (k, _) in idle.into_iter().take(table.len() - max) {
            table.remove(&k);
        }
    }
    drop(table);
    state
        .slot_reserve
        .lock_or_recover()
        .retain(|_, e| now.saturating_duration_since(e.at) < SLOT_RESERVE_MAX);
}

/// Backends come and go: forget every remembered placement.
pub fn clear_affinity(state: &AppState) {
    state.affinity.lock_or_recover().clear();
}

/// Longest a user with nothing queued and nothing in flight is kept in the
/// per-user bookkeeping.
const USER_RETENTION: std::time::Duration = std::time::Duration::from_secs(3600);

/// Hard ceiling on tracked users; the least recently seen idle ones are
/// dropped beyond it, whatever their retention.
const MAX_TRACKED_USERS: usize = 10_000;

/// How often idle users are pruned.
const USER_PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Most requests one user may have waiting before further ones are refused.
const MAX_QUEUED_PER_USER: usize = 100;

/// Body limit for `/api/blobs/{digest}`, which carries raw model layers rather
/// than JSON. Every other route uses `settings.max_body_bytes`.
pub const BLOB_BODY_LIMIT: usize = 1024 * 1024 * 1024;

/// How many log events to keep in the ring buffer.
const MAX_LOG_EVENTS: usize = 300;
/// How many of the newest events may keep their (potentially large) content; older ones have it stripped to bound memory.
const MAX_LOG_CONTENT_EVENTS: usize = 100;

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backends: Vec<(String, Option<String>)>,
        timeout: u64,
        load_keep_alive: i64,
        stuck_timeout_secs: u64,
        max_concurrent_per_backend: u32,
        model_config_path: String,
        model_config: Vec<crate::config::ModelConfig>,
        reqlog: crate::reqlog::RequestLogger,
        log_content_limit: usize,
        max_queued_bytes: u64,
    ) -> Self {
        let (blocked_ips, blocked_users) = Self::load_blocked_items();
        // Which declared `models:` entries belong to which backend, resolved
        // once here with the same selector the loader uses. The names are not
        // put into `available_models` here: `apply_probe` stays the single
        // admission rule (see `BackendStatus::declared_models`), and the health
        // loop's first round runs immediately at startup.
        let urls: Vec<String> = backends.iter().map(|(u, _)| u.clone()).collect();
        let declared = crate::control::declared_models_for(&urls, &model_config);
        let backends = backends
            .into_iter()
            .enumerate()
            .map(|(i, (url, token))| {
                BackendStatus::new(url, token, declared.get(i).cloned().unwrap_or_default())
            })
            .collect();

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout))
            .build()
            .unwrap();

        Self {
            queues: Mutex::new(HashMap::new()),
            processing_counts: Mutex::new(HashMap::new()),
            processed_counts: Mutex::new(HashMap::new()),
            dropped_counts: Mutex::new(HashMap::new()),
            user_ips: Mutex::new(HashMap::new()),
            blocked_ips: Mutex::new(blocked_ips),
            blocked_users: Mutex::new(blocked_users),
            vip_user: Mutex::new(None),
            boost_user: Mutex::new(None),
            global_counter: std::sync::atomic::AtomicUsize::new(0),
            fair_share: Mutex::new(HashMap::new()),
            affinity: Mutex::new(HashMap::new()),
            conv_cache: Mutex::new(HashMap::new()),
            slot_reserve: Mutex::new(HashMap::new()),
            sticky_mode: std::sync::atomic::AtomicU8::new(
                crate::config::StickyMode::default() as u8,
            ),
            sticky_ttl: std::sync::atomic::AtomicU64::new(1800),
            sticky_lag: std::sync::atomic::AtomicU64::new(0),
            sticky_max_entries: std::sync::atomic::AtomicU64::new(10_000),
            notify: Notify::new(),
            backend_freed: Notify::new(),
            backends: Mutex::new(backends),
            last_backend_idx: std::sync::atomic::AtomicUsize::new(0),
            timeout: std::sync::atomic::AtomicU64::new(timeout),
            stuck_timeout_secs: std::sync::atomic::AtomicU64::new(stuck_timeout_secs),
            client,
            control_ops: Mutex::new(HashMap::new()),
            control_history: Mutex::new(VecDeque::new()),
            load_keep_alive: std::sync::atomic::AtomicI64::new(load_keep_alive),
            max_concurrent_per_backend: std::sync::atomic::AtomicU32::new(max_concurrent_per_backend),
            detect_slots: std::sync::atomic::AtomicBool::new(true),
            model_aliases: Mutex::new(HashMap::new()),
            ip_identity: std::sync::atomic::AtomicBool::new(true),
            model_limits: Mutex::new(build_model_limits(&model_config)),
            model_config: Mutex::new(model_config),
            model_config_path,
            logs: Mutex::new(VecDeque::new()),
            reqlog,
            log_content_limit: std::sync::atomic::AtomicUsize::new(log_content_limit),
            max_queued_bytes: std::sync::atomic::AtomicU64::new(max_queued_bytes),
            allow_all_routes: std::sync::atomic::AtomicBool::new(false),
            metrics_token: Mutex::new(None),
            config: Mutex::new(crate::settings::Layers::default()),
            queued_bytes: std::sync::atomic::AtomicU64::new(0),
            clients: Mutex::new(crate::clients::ClientStore::in_memory()),
            model_policy: Mutex::new(crate::model_policy::ModelPolicyStore::in_memory()),
            metrics: crate::metrics::Metrics::new(),
            runs: crate::metrics::Runs::default(),
            stats: crate::stats::Stats::default(),
            logs_evicted: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Pin the API family of backends whose `appconf.yaml` entry carried a
    /// `kind:` (`[(url, token)]` order is the config order — the caller passes
    /// one entry per backend, in the same order, empty when the backends came
    /// from `--backend-urls`). An empty/unknown `kind:` leaves that backend to
    /// probe classification.
    ///
    /// Must run BEFORE the health loop starts: the pin is what makes an
    /// undetectable backend routable from the first probe round, and applying it
    /// later would let a round classify it as `???` in between.
    pub fn apply_backend_kinds(&self, kinds: &[Option<String>]) {
        let mut backends = self.backends.lock_or_recover();
        for (b, kind) in backends.iter_mut().zip(kinds) {
            let Some(pinned) = ApiSet::from_kind(kind.as_deref()) else {
                if let Some(k) = kind.as_deref().filter(|k| !k.trim().is_empty()) {
                    warn!(
                        "Backend {} has unknown kind '{}' — classifying it from probes instead",
                        b.url, k
                    );
                }
                continue;
            };
            b.forced_api = pinned;
            b.api_type = pinned;
            info!(
                "Backend {} kind pinned to {} (probes will not override it)",
                b.url,
                pinned.display()
            );
        }
    }

    fn load_blocked_items() -> (HashSet<IpAddr>, HashSet<String>) {
        if let Ok(content) = fs::read_to_string(BLOCKED_FILE)
            && let Ok(config) = serde_json::from_str::<BlockedConfig>(&content)
        {
            return (config.ips, config.users);
        }
        (HashSet::new(), HashSet::new())
    }

    fn save_blocked_items(&self) {
        // The file lives in the working directory: tests must not write it
        // into the source tree.
        if cfg!(test) {
            return;
        }
        let config = BlockedConfig {
            ips: self.blocked_ips.lock_or_recover().clone(),
            users: self.blocked_users.lock_or_recover().clone(),
        };
        if let Ok(content) = serde_json::to_string_pretty(&config) {
            let _ = fs::write(BLOCKED_FILE, content);
        }
    }

    pub fn block_ip(&self, ip: IpAddr) {
        {
            let mut ips = self.blocked_ips.lock_or_recover();
            ips.insert(ip);
        }
        self.save_blocked_items();
        warn!("IP blocked: {}", ip);
    }

    /// Block a queue identity. A registered client is blocked by disabling
    /// it (saved with the client), so there is one switch per client rather
    /// than a user block and a client flag that can disagree.
    pub fn block_user(&self, user_id: String) {
        {
            let mut clients = self.clients.lock_or_recover();
            if clients.is_registered(&user_id) {
                match clients.set_disabled(&user_id, true) {
                    Ok(()) => warn!("Client disabled: {}", user_id),
                    Err(e) => warn!("Cannot disable client {}: {}", user_id, e),
                }
                return;
            }
        }
        {
            let mut users = self.blocked_users.lock_or_recover();
            users.insert(user_id.clone());
        }
        self.save_blocked_items();
        warn!("User blocked: {}", user_id);
    }

    #[allow(dead_code)]
    pub fn unblock_ip(&self, ip: IpAddr) {
        {
            let mut ips = self.blocked_ips.lock_or_recover();
            ips.remove(&ip);
        }
        self.save_blocked_items();
        info!("IP unblocked: {}", ip);
    }

    /// Lift a block; for a registered client this enables it again.
    pub fn unblock_user(&self, user_id: &str) {
        {
            let mut clients = self.clients.lock_or_recover();
            if clients.is_disabled(user_id) {
                match clients.set_disabled(user_id, false) {
                    Ok(()) => info!("Client enabled: {}", user_id),
                    Err(e) => warn!("Cannot enable client {}: {}", user_id, e),
                }
            }
        }
        let removed = self.blocked_users.lock_or_recover().remove(user_id);
        if removed {
            self.save_blocked_items();
            info!("User unblocked: {}", user_id);
        }
    }

    /// Toggle `user_id` as the VIP user. A user is never VIP and boost at
    /// once, so becoming VIP clears their boost.
    pub fn toggle_vip(&self, user_id: &str) {
        {
            let mut vip = self.vip_user.lock_or_recover();
            if vip.as_deref() == Some(user_id) {
                *vip = None;
            } else {
                *vip = Some(user_id.to_string());
            }
        }
        {
            let mut boost = self.boost_user.lock_or_recover();
            if boost.as_deref() == Some(user_id) {
                *boost = None;
            }
        }
        self.persist_priority();
    }

    /// Toggle `user_id` as the boost user; the mirror of [`Self::toggle_vip`].
    pub fn toggle_boost(&self, user_id: &str) {
        {
            let mut boost = self.boost_user.lock_or_recover();
            if boost.as_deref() == Some(user_id) {
                *boost = None;
            } else {
                *boost = Some(user_id.to_string());
            }
        }
        {
            let mut vip = self.vip_user.lock_or_recover();
            if vip.as_deref() == Some(user_id) {
                *vip = None;
            }
        }
        self.persist_priority();
    }

    pub fn is_ip_blocked(&self, ip: &IpAddr) -> bool {
        self.blocked_ips.lock_or_recover().contains(ip)
    }

    /// How many requests this backend may run at once, and whether that came
    /// from the backend itself: the slots it reports (llama.cpp, halogen)
    /// when detection is on, else `settings.max_concurrent_per_backend`.
    pub fn backend_capacity(&self, b: &BackendStatus) -> (u32, bool) {
        match b.detected_slots {
            Some(n) if self.detect_slots.load(std::sync::atomic::Ordering::Relaxed) => (n, true),
            _ => (self.max_concurrent_per_backend.load(std::sync::atomic::Ordering::Relaxed), false),
        }
    }

    /// How long a request may wait when no backend can ever serve it.
    pub fn stuck_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.stuck_timeout_secs.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Requests a backend could start now: capacity minus in-flight, none
    /// while offline or while a model load/unload runs on it (the scheduler
    /// treats that backend as busy).
    pub fn free_slots(&self, b: &BackendStatus, control_busy: bool) -> usize {
        if !b.is_online || control_busy {
            return 0;
        }
        (self.backend_capacity(b).0 as usize).saturating_sub(b.active_requests)
    }

    /// Replace the model aliases (keys matched case-insensitively).
    pub fn set_model_aliases(&self, aliases: Option<&std::collections::BTreeMap<String, String>>) {
        *self.model_aliases.lock_or_recover() = aliases
            .map(|a| {
                a.iter()
                    .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
                    .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                    .collect()
            })
            .unwrap_or_default();
    }

    /// The body with its `model` swapped for the alias target, when the
    /// request names an alias; otherwise the body untouched (no re-encoding).
    pub fn apply_model_alias(&self, body: Bytes) -> Bytes {
        let target = {
            let aliases = self.model_aliases.lock_or_recover();
            if aliases.is_empty() {
                return body;
            }
            match extract_requested_model(&body)
                .and_then(|m| aliases.get(&m.trim().to_lowercase()).cloned())
            {
                Some(t) => t,
                None => return body,
            }
        };
        let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(&body) else {
            return body;
        };
        let Some(obj) = json.as_object_mut() else {
            return body;
        };
        debug!("model alias: {:?} -> {}", obj.get("model"), target);
        obj.insert("model".to_string(), serde_json::Value::String(target));
        serde_json::to_vec(&json).map(Bytes::from).unwrap_or(body)
    }

    /// The `model` metric label for a request (see [`model_label_in`]).
    pub fn model_label(&self, requested: Option<&str>) -> String {
        model_label_in(&self.backends.lock_or_recover(), requested)
    }

    /// The current model policy, without holding its lock afterwards.
    pub fn model_policy(&self) -> Arc<crate::model_policy::Policy> {
        self.model_policy.lock_or_recover().snapshot()
    }

    /// True when the policy keeps every backend from serving `requested`, so
    /// the request can be answered `404` before it is queued. A model no
    /// backend lists at all is left to the normal path unless a rule names
    /// it directly.
    pub fn model_blocked(&self, requested: &str) -> bool {
        let policy = self.model_policy();
        if policy.is_open() {
            return false;
        }
        let backends: Vec<(String, Arc<HashSet<String>>)> = self
            .backends
            .lock_or_recover()
            .iter()
            .map(|b| (b.url.clone(), b.available_models.clone()))
            .collect();
        let mut listed = false;
        for (url, available) in &backends {
            if available.iter().any(|m| name_matches_listed(requested, m)) {
                listed = true;
                if policy_allows(&policy, requested, url, available) {
                    return false;
                }
            }
        }
        if listed {
            return true;
        }
        !backends.is_empty() && !backends.iter().any(|(url, _)| policy.served(requested, url))
    }

    /// Blocked user id, or disabled registered client.
    pub fn is_user_blocked(&self, user_id: &str) -> bool {
        // Separate statements: the two locks are never held together.
        let blocked = self.blocked_users.lock_or_recover().contains(user_id);
        blocked || self.clients.lock_or_recover().is_disabled(user_id)
    }

    /// The queue identity for a request without a client key, from its
    /// `X-User-ID`. Registered client names are reserved for requests that
    /// present that client's key: a keyless claim of one is queued apart, as
    /// `unverified:<name>`, so it gets none of the client's queue position,
    /// priority or stats (client names cannot contain ':', so this never
    /// collides with a real client).
    pub fn unkeyed_identity(&self, claimed: &str) -> String {
        if self.clients.lock_or_recover().is_registered(claimed) {
            format!("unverified:{claimed}")
        } else {
            claimed.to_string()
        }
    }

    /// Save which registered clients hold VIP / boost.
    fn persist_priority(&self) {
        let vip = self.vip_user.lock_or_recover().clone();
        let boost = self.boost_user.lock_or_recover().clone();
        if let Err(e) = self
            .clients
            .lock_or_recover()
            .set_priority(vip.as_deref(), boost.as_deref())
        {
            warn!("Cannot save VIP/boost: {}", e);
        }
    }

    /// Startup: restore VIP / boost from the client store, and turn user
    /// blocks on registered names (from before blocking a client meant
    /// disabling it) into disabled clients.
    pub fn apply_client_store(&self) {
        let (vip, boost) = self.clients.lock_or_recover().priority();
        *self.vip_user.lock_or_recover() = vip;
        *self.boost_user.lock_or_recover() = boost;

        let names = self.clients.lock_or_recover().names();
        let migrate: Vec<String> = {
            let blocked = self.blocked_users.lock_or_recover();
            names.into_iter().filter(|n| blocked.contains(n)).collect()
        };
        if migrate.is_empty() {
            return;
        }
        for name in &migrate {
            if let Err(e) = self.clients.lock_or_recover().set_disabled(name, true) {
                warn!("Cannot disable client {}: {}", name, e);
                continue;
            }
            self.blocked_users.lock_or_recover().remove(name);
            info!("User block on client {} converted to disabled client", name);
        }
        self.save_blocked_items();
    }

    /// Move a renamed client's live state to its new name: queued requests,
    /// counters, last IP, fair-share score, VIP / boost. Requests already in
    /// flight finish under the old name (their accounting holds that key).
    pub fn rename_identity(&self, old: &str, new: &str) {
        fn move_key<V>(map: &mut HashMap<String, V>, old: &str, new: &str) {
            if let Some(v) = map.remove(old) {
                map.insert(new.to_string(), v);
            }
        }
        {
            let mut queues = self.queues.lock_or_recover();
            if let Some(mut q) = queues.remove(old) {
                // Keep anything that raced in under the new name behind it.
                if let Some(newer) = queues.remove(new) {
                    q.extend(newer);
                }
                queues.insert(new.to_string(), q);
            }
        }
        move_key(&mut self.processed_counts.lock_or_recover(), old, new);
        move_key(&mut self.dropped_counts.lock_or_recover(), old, new);
        move_key(&mut self.user_ips.lock_or_recover(), old, new);
        move_key(&mut self.fair_share.lock_or_recover(), old, new);
        for slot in [&self.vip_user, &self.boost_user] {
            let mut v = slot.lock_or_recover();
            if v.as_deref() == Some(old) {
                *v = Some(new.to_string());
            }
        }
        self.persist_priority();
        self.notify.notify_one();
    }

    /// A client was just created. If its name was already in use as a plain
    /// id, carry that id's settings over: a user block becomes "disabled", and
    /// VIP / boost become saved with the client.
    pub fn client_registered(&self, name: &str) {
        let was_blocked = self.blocked_users.lock_or_recover().remove(name);
        if was_blocked {
            self.save_blocked_items();
            if let Err(e) = self.clients.lock_or_recover().set_disabled(name, true) {
                warn!("Cannot disable client {}: {}", name, e);
            }
        }
        self.persist_priority();
    }

    /// A client was deleted: its name is an ordinary id again, so it loses
    /// the client's VIP / boost.
    pub fn forget_client(&self, name: &str) {
        for slot in [&self.vip_user, &self.boost_user] {
            let mut v = slot.lock_or_recover();
            if v.as_deref() == Some(name) {
                *v = None;
            }
        }
    }

    /// Append an event to the logs ring buffer (bounded).
    pub fn log_event(&self, ev: LogEvent) {
        let mut logs = self.logs.lock_or_recover();
        logs.push_back(ev);
        while logs.len() > MAX_LOG_EVENTS {
            logs.pop_front();
            // Only ever changed under the `logs` lock, so it is consistent
            // with the ring whenever the ring is read.
            self.logs_evicted
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // Bound memory: only the newest MAX_LOG_CONTENT_EVENTS may keep
        // their (potentially large) content — strip it from older events,
        // walking the ring from the front (oldest first).
        let mut content_events = logs.iter().filter(|e| e.content.is_some()).count();
        let mut i = 0;
        while content_events > MAX_LOG_CONTENT_EVENTS {
            if let Some(e) = logs.get_mut(i).filter(|e| e.content.is_some()) {
                e.content = None;
                content_events -= 1;
            }
            i += 1;
        }
    }
}

/// Per-pair normalization used by both the set-level matcher and model-name
/// resolution in `control.rs`: compares names ignoring the `:tag` suffix and
/// case (so `llama3` matches `llama3:latest`).
pub fn smart_model_match_one(requested: &str, model: &str) -> bool {
    let requested_low = requested.to_lowercase();
    let requested_no_tag = requested_low.split(':').next().unwrap_or(&requested_low);
    let model_low = model.to_lowercase();
    let model_no_tag = model_low.split(':').next().unwrap_or(&model_low);
    requested_no_tag == model_no_tag
}

/// Bounded normalization for model-name comparison: lowercase, drop the
/// `:tag` suffix and any LM Studio-style `@quant` suffix.
fn normalize_model_id(id: &str) -> String {
    let low = id.to_lowercase();
    let no_tag = low.split(':').next().unwrap_or(&low);
    no_tag.split('@').next().unwrap_or(no_tag).to_string()
}

/// If a model config entry pins `requested` (by normalized name or identifier)
/// to a non-empty backend list, return that list. First matching entry wins
/// (config file order). Returns None when the model is not pinned.
fn model_pin_for(requested: &str, configs: &[crate::config::ModelConfig]) -> Option<Vec<String>> {
    let requested_norm = normalize_model_id(requested);
    configs
        .iter()
        .find(|c| {
            !c.backends.is_empty()
                && (requested_norm == normalize_model_id(&c.name)
                    || c.identifier
                        .as_deref()
                        .map(|id| !id.is_empty() && requested_norm == normalize_model_id(id))
                        .unwrap_or(false))
        })
        .map(|c| c.backends.clone())
}

/// Case-insensitive substring backend URL match for pin routing: the pin
/// URL is normalized with `trim_end_matches('/')` + `to_lowercase()`, and a
/// backend matches when its (lowercased) URL contains it — consistent with
/// `control::config_targets` load targeting, so the same `backends:` entry
/// that loads a model also routes requests for it.
fn pin_url_matches(pin_url: &str, backend_url: &str) -> bool {
    let pin_low = pin_url.trim_end_matches('/').to_lowercase();
    backend_url.to_lowercase().contains(&pin_low)
}

/// Publisher-agnostic part of a normalized id (`owner/model` -> `model`).
fn model_base(id: &str) -> String {
    let n = normalize_model_id(id);
    match n.rsplit_once('/') {
        Some((_, m)) => m.to_string(),
        None => n,
    }
}

/// Key under which per-model concurrency is both limited and counted.
///
/// Every site that reads `models[].max_concurrent_requests` or touches
/// `active_by_model` MUST use this one function. Keying the limits by one
/// normalization and the in-flight counters by another meant a config entry
/// `qwen/qwen3.8-27b` never matched a client asking for `qwen3.8-27b`: the
/// limit silently fell back to 1, and the two spellings accumulated in
/// separate counters, so the cap could be exceeded on a single backend.
///
/// It is the publisher-stripped base name — the same normalization the routing
/// rules apply to bare requests — so two publishers' builds of one model share
/// a budget. That errs toward protecting the backend.
fn model_concurrency_key(id: &str) -> String {
    model_base(id)
}

/// True when `requested` matches `listed` under the bounded rules shared by
/// `model_routable`: tag/case-insensitive equality (`smart_model_match_one`),
/// or equality after full normalization (lowercase, strip `:tag` and `@quant`),
/// or — for bare requested names only — publisher-stripped base equality.
fn name_matches_listed(requested: &str, listed: &str) -> bool {
    if smart_model_match_one(requested, listed)
        || normalize_model_id(listed) == normalize_model_id(requested)
    {
        return true;
    }
    if !requested.contains('/') {
        return model_base(requested) == model_base(listed);
    }
    false
}

/// Scheduler-side routability check. Deterministic and bounded — no arbitrary
/// substring matching (which could route `qwen3.8-27b` to an unrelated
/// `huihui-qwen3.8-27b-abliterated`). A backend is eligible when any listed id
/// matches via `name_matches_listed`:
/// - equals the request after tag/case normalization (`smart_model_match_one`), or
/// - equals it after full normalization (handles `@quant` suffixes on either side), or,
/// - for bare requested names only — equals its publisher-stripped base, so a
///   client asking `llama3` can reach `meta-llama/llama3:latest`.
///
/// Owner-prefixed requests are strict: the backend must list that exact id up to
/// tag/quant normalization.
pub(crate) fn model_routable(requested: &str, available: &HashSet<String>) -> bool {
    available.iter().any(|m| name_matches_listed(requested, m))
}

/// The route behind an Olla-style path: `/olla/openai/...` and
/// `/olla/proxy/...` map to `/...`; anything else is unchanged.
pub fn strip_olla_prefix(path: &str) -> &str {
    for prefix in ["/olla/openai", "/olla/proxy"] {
        if let Some(rest) = path.strip_prefix(prefix)
            && rest.starts_with('/')
        {
            return rest;
        }
    }
    path
}

/// TTFT for non-streamed responses, and generation / prefill speed, once a
/// response has finished; also noted in `timing` for the Overview. Backend timings win; for streams without them,
/// generation time is measured from the first token to the end.
fn record_speed(
    metrics: &crate::metrics::Metrics,
    [client, model, backend]: [&str; 3],
    usage: Option<crate::metrics::Usage>,
    first_token: Option<std::time::Instant>,
    live_tokens: u64,
    timing: &mut crate::stats::Timing,
) {
    use crate::metrics::{GEN_SECONDS, GEN_SPEED, GEN_TOKENS, PROMPT_EVAL_TOKENS, PROMPT_SECONDS, TTFT};
    let labels = [client, model, backend];
    let u = usage.unwrap_or_default();
    if first_token.is_none()
        && let Some(p) = u.prompt_secs
    {
        metrics.observe(&TTFT, &labels, p);
        timing.ttft = Some(p);
    }
    if let (Some(secs), Some(n)) = (u.prompt_secs, u.prompt_eval)
        && n > 0
    {
        metrics.inc(&PROMPT_SECONDS, &labels, secs);
        metrics.inc(&PROMPT_EVAL_TOKENS, &labels, n as f64);
        timing.prefill = Some((secs, n));
    }
    let tokens = if usage.is_some() { u.completion } else { live_tokens };
    let secs = u
        .gen_secs
        .or_else(|| first_token.map(|t| t.elapsed().as_secs_f64()));
    // One token has no measurable rate.
    if let Some(secs) = secs.filter(|s| *s > 0.0)
        && tokens >= 2
    {
        metrics.inc(&GEN_SECONDS, &labels, secs);
        metrics.inc(&GEN_TOKENS, &labels, tokens as f64);
        metrics.observe(&GEN_SPEED, &[model, backend], tokens as f64 / secs);
        timing.decode = Some((secs, tokens));
    }
}

/// The `endpoint` metric label: the route template `path` belongs to, so
/// path parameters (blob digests, model ids) never become label values.
pub fn route_template(path: &str) -> &'static str {
    const ROUTES: &[&str] = &[
        "/",
        "/api/generate",
        "/api/chat",
        "/api/embed",
        "/api/embeddings",
        "/api/tags",
        "/api/show",
        "/api/create",
        "/api/copy",
        "/api/delete",
        "/api/pull",
        "/api/push",
        "/api/ps",
        "/api/version",
        "/v1/chat/completions",
        "/v1/completions",
        "/v1/embeddings",
        "/v1/models",
        "/v1/systemone",
    ];
    if let Some(r) = ROUTES.iter().find(|r| **r == path) {
        return r;
    }
    if path.starts_with("/api/blobs/") {
        return "/api/blobs/{digest}";
    }
    if path.starts_with("/v1/models/") {
        return "/v1/models/{model}";
    }
    "other"
}

/// The `model` metric label: the requested name when some backend lists it,
/// `unknown` for names none does (typos, probes — clients choose these, so
/// they must not become label values), `none` for model-less requests.
/// Takes the backend list so callers already holding its lock can use it.
pub fn model_label_in(backends: &[BackendStatus], requested: Option<&str>) -> String {
    match requested {
        None => "none".to_string(),
        Some(m) if backends.iter().any(|b| model_routable(m, &b.available_models)) => m.to_string(),
        Some(_) => "unknown".to_string(),
    }
}

/// `404` for a hidden model, in the error shape of the API family the path
/// belongs to, so clients treat it like any model the server does not have.
fn model_not_found(path: &str, model: &str) -> axum::response::Response {
    let message = format!("model '{model}' not found");
    let body = if path.starts_with("/v1/") {
        serde_json::json!({ "error": {
            "message": message,
            "type": "invalid_request_error",
            "param": "model",
            "code": "model_not_found",
        }})
    } else {
        serde_json::json!({ "error": message })
    };
    (StatusCode::NOT_FOUND, axum::Json(body)).into_response()
}

/// Whether the model policy lets the backend at `url` serve `requested`.
///
/// Rules are written against the names backends list, so the check runs on
/// the listed names the request resolves to (with the same matching as
/// routing): a request for `llama3` is refused when the listed
/// `llama3:latest` is hidden. A hide rule naming the requested spelling
/// itself also refuses it. A backend that lists nothing matching is judged
/// on the requested name.
fn policy_allows(
    policy: &crate::model_policy::Policy,
    requested: &str,
    url: &str,
    available: &HashSet<String>,
) -> bool {
    if policy.is_open() {
        return true;
    }
    if policy.hidden(requested, url) {
        return false;
    }
    let mut matched = false;
    for m in available.iter().filter(|m| name_matches_listed(requested, m)) {
        matched = true;
        if policy.served(m, url) {
            return true;
        }
    }
    !matched && policy.served(requested, url)
}

/// True when the requested model is resident on this backend per its latest
/// probe: either in `loaded_models` (Ollama `/api/ps`, LM Studio loaded
/// instance keys/ids) or — for LM Studio — a native-list entry with loaded
/// instances whose key or display name matches the request.
pub fn model_loaded_on(b: &BackendStatus, requested: &str) -> bool {
    if model_routable(requested, &b.loaded_models) {
        return true;
    }
    if b.lmstudio {
        return b.native_models.iter().any(|m| {
            !m.loaded_instance_ids.is_empty()
                && (name_matches_listed(requested, &m.key)
                    || m.display_name
                        .as_deref()
                        .map(|d| name_matches_listed(requested, d))
                        .unwrap_or(false))
        });
    }
    false
}

/// Per-model concurrency limits keyed by `model_concurrency_key`, from the
/// `models:` section of appconf.yaml. Unlisted models default to 1 at lookup.
pub fn build_model_limits(configs: &[crate::config::ModelConfig]) -> HashMap<String, u32> {
    configs
        .iter()
        .map(|c| (model_concurrency_key(&c.name), c.max_concurrent_requests))
        .collect()
}

/// True when the backend has room for one more in-flight request: under the
/// global per-backend cap, and — for model requests — under that model's limit.
fn backend_has_capacity(
    b: &BackendStatus,
    model_key: Option<&str>,
    model_limit: u32,
    cap: u32,
) -> bool {
    if b.active_requests >= cap as usize {
        return false;
    }
    match model_key {
        Some(k) => *b.active_by_model.get(k).unwrap_or(&0) < model_limit,
        None => true,
    }
}

/// Which families this backend could serve `route` with: what it speaks ∩ what
/// the route accepts. A backend that has not been classified yet (`api_type`
/// empty) is a candidate for every family the route accepts, exactly as the old
/// `Unknown` variant was.
fn route_candidates(b: &BackendStatus, route: ApiSet) -> ApiSet {
    if b.api_type.is_empty() {
        route
    } else {
        b.api_type.intersection(route)
    }
}

/// True when the backend can speak the request's API dialect. Unknown-type
/// backends are allowed until the health check classifies them; families this
/// backend has been observed rejecting in real traffic (see
/// `apply_family_learning`) are excluded until a request of that family
/// succeeds or the backend restarts.
fn family_compatible(b: &BackendStatus, route: ApiSet) -> bool {
    let candidates = route_candidates(b, route);
    if candidates.is_empty() {
        return false;
    }
    // Excluded only when the backend has been seen rejecting *every* family
    // this route could be served by — a `/v1/models` request is answered by
    // OpenAI and System One backends alike, so one rejecting family must not
    // disqualify a backend that still serves the other.
    !candidates
        .members()
        .all(|family| b.rejected_families.contains(&family))
}

/// Which single family a response on `route` teaches something about, if any.
///
/// For every route that existed before the System One family this is exactly
/// the family the old prefix rule named, so learning behaviour is unchanged.
/// A multi-family route (`GET /v1/models`) is only attributable when the
/// backend's own detected type pins it to one of the two, and a catch-all route
/// (`/`) or an unclassified backend teaches nothing — a wrong strike removes a
/// healthy backend from rotation for that family.
fn learnable_family(b: &BackendStatus, route: ApiSet) -> Option<ApiFamily> {
    if let Some(family) = route.single() {
        return Some(family);
    }
    if route == ApiSet::ALL || b.api_type.is_empty() {
        return None;
    }
    b.api_type.intersection(route).single()
}

/// Heuristic: did this response reject the request's API *dialect* (as opposed
/// to a normal application error like model-not-found)? Conservative on purpose
/// — only structural endpoint/method rejections count, so ordinary errors never
/// poison the scheduler.
fn is_endpoint_rejection(status: StatusCode, body_prefix: &str) -> bool {
    // 405 Method Not Allowed is always structural.
    if status.as_u16() == 405 {
        return true;
    }
    let text = body_prefix.trim();
    if !text.starts_with('{') {
        return false;
    }
    // The body must parse as JSON *in this prefix*. A dialect rejection is a
    // short error object, so it always fits; a prefix that is truncated
    // mid-JSON therefore belongs to some long body — a streaming completion,
    // say — and must not be scanned for "endpoint"/"method" as raw text. Doing
    // so let an assistant reply that happened to be cut mid-sentence while
    // discussing HTTP methods count as a strike against the backend, and two
    // strikes drop it from rotation for that API family.
    let Some(value) = serde_json::from_str::<serde_json::Value>(text).ok() else {
        return false;
    };
    let Some(error_text) = value.get("error").and_then(|e| e.as_str()) else {
        return false;
    };
    let error_text = error_text.to_lowercase();
    error_text.contains("endpoint") || error_text.contains("method")
}

/// Record a traffic observation for one backend + API family. Two consecutive
/// endpoint rejections mark the family as rejected (the scheduler then excludes
/// this backend for that family); any non-rejection response clears the memory.
fn apply_family_learning(state: &AppState, backend_idx: usize, route: ApiSet, rejected: bool) {
    let mut backends = state.backends.lock_or_recover();
    let Some(b) = backends.get_mut(backend_idx) else {
        return;
    };
    // Nothing to learn from a path no family owns, or one the answer is
    // ambiguous about (see `learnable_family`).
    let Some(family) = learnable_family(b, route) else {
        return;
    };
    if rejected {
        let count = b.family_fail_counts.entry(family).or_insert(0);
        *count += 1;
        if *count >= 2 && !b.rejected_families.contains(&family) {
            b.rejected_families.insert(family);
            info!(
                "Backend {} learned: rejecting {}-family requests ({} consecutive endpoint errors)",
                b.url,
                family.display(),
                count
            );
        }
    } else if b.family_fail_counts.remove(&family).is_some()
        || b.rejected_families.remove(&family)
    {
        debug!(
            "Backend {} serves {}-family requests again; cleared rejection memory",
            b.url,
            family.display()
        );
    }
}

/// Give back the queue-memory budget held by a task that has left a queue.
/// Saturating, so a task queued outside the normal enqueue path can never wrap
/// the counter and lock out every later request.
fn release_queued_bytes(state: &AppState, bytes: u64) {
    let _ = state.queued_bytes.fetch_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |held| Some(held.saturating_sub(bytes)),
    );
}

/// Drop per-user bookkeeping for users that are gone.
///
/// Every one of these maps is keyed by the client-supplied `X-User-ID` header
/// and nothing ever removed from them, so a client could grow them without
/// bound just by varying the header. A user is prunable only while nothing of
/// theirs is queued or in flight; beyond that they are kept for
/// [`USER_RETENTION`] so the dashboard still shows recent activity, and the
/// least recently seen are dropped early once more than [`MAX_TRACKED_USERS`]
/// are tracked.
fn prune_idle_users(state: &AppState, retention: std::time::Duration) {
    let now = std::time::Instant::now();

    // Candidates: nothing queued, nothing being served.
    let mut idle: Vec<(String, std::time::Instant)> = {
        let queues = state.queues.lock_or_recover();
        let processing = state.processing_counts.lock_or_recover();
        let scores = state.fair_share.lock_or_recover();
        scores
            .iter()
            .filter(|(u, _)| {
                queues.get(*u).is_none_or(|q| q.is_empty())
                    && processing.get(*u).copied().unwrap_or(0) == 0
            })
            .map(|(u, s)| (u.clone(), s.last_seen()))
            .collect()
    };
    if idle.is_empty() {
        return;
    }

    // Stalest first, so an over-cap trim drops the least recently seen.
    idle.sort_by_key(|(_, seen)| *seen);
    let tracked = state.fair_share.lock_or_recover().len();
    let over_cap = tracked.saturating_sub(MAX_TRACKED_USERS);

    let mut remove: Vec<String> = idle
        .iter()
        .enumerate()
        .filter(|(i, (_, seen))| *i < over_cap || now.saturating_duration_since(*seen) > retention)
        .map(|(_, (user, _))| user.clone())
        .collect();
    if remove.is_empty() {
        return;
    }

    // Re-check emptiness while removing: a request may have arrived since the
    // candidate list was built, and dropping a queue with a task still in it
    // would lose that request.
    {
        let mut queues = state.queues.lock_or_recover();
        remove.retain(|u| queues.get(u).is_none_or(|q| q.is_empty()));
        for u in &remove {
            queues.remove(u);
        }
    }
    for u in &remove {
        state.processing_counts.lock_or_recover().remove(u);
        state.processed_counts.lock_or_recover().remove(u);
        state.dropped_counts.lock_or_recover().remove(u);
        state.user_ips.lock_or_recover().remove(u);
        state.fair_share.lock_or_recover().remove(u);
    }
    debug!("pruned {} idle user(s) from per-user state", remove.len());
}

/// One pass of the background health check: probe every backend and merge the
/// results into `state`.
///
/// Backends are probed CONCURRENTLY. Probing them one after another let a
/// single unresponsive backend hold up everyone else's status for the whole
/// round — combined with the probe timeout (see [`crate::control::PROBE_TIMEOUT`])
/// that could stall the loop for minutes while the scheduler routed on stale
/// online/loaded state.
/// One backend to probe in a health round: (index, url, token,
/// was_online, known-bad endpoints to skip).
type ProbeTarget = (usize, String, Option<String>, bool, HashSet<String>);

async fn health_check_round(
    state: &Arc<AppState>,
    client: &reqwest::Client,
    full_reprobe: bool,
    probe_timeout: std::time::Duration,
) {
    // Snapshot what to probe, and which endpoints to skip, in one lock pass.
    let to_check: Vec<ProbeTarget> = {
        let backends = state.backends.lock_or_recover();
        backends
            .iter()
            .enumerate()
            // Disabled backends stay offline and unprobed.
            .filter(|(_, b)| !b.disabled)
            .map(|(i, b)| {
                // Skip endpoints this backend is known to reject — except on a
                // full re-probe or right after recovery from offline (memory
                // may be stale after a restart). Never skip both primary API
                // probes: online status must stay re-establishable.
                let skip = if full_reprobe || !b.is_online {
                    HashSet::new()
                } else {
                    let mut s = b.known_bad_endpoints.clone();
                    if s.contains("/api/tags") && s.contains("/v1/models") {
                        s.clear();
                    }
                    s
                };
                (i, b.url.clone(), b.token.clone(), b.is_online, skip)
            })
            .collect()
    };

    let probes = futures_util::future::join_all(to_check.into_iter().map(
        |(idx, url, token, was_online, skip)| {
            let client = client.clone();
            async move {
                let probe = crate::control::probe_backend(
                    &client,
                    &url,
                    token.as_deref(),
                    &skip,
                    probe_timeout,
                )
                .await;
                (idx, url, was_online, probe)
            }
        },
    ))
    .await;

    let mut any_changed = false;
    for (idx, url, was_online, probe) in probes {
        let mut backends = state.backends.lock_or_recover();
        let Some(b) = backends.get_mut(idx) else {
            continue;
        };

        // A backend that just recovered from offline may have been
        // restarted: clear learned endpoint/family memory so it is
        // re-learned from scratch.
        if !was_online && probe.is_online {
            b.known_bad_endpoints.clear();
            b.rejected_families.clear();
            b.family_fail_counts.clear();
        }

        let newly_bad: Vec<String> = probe
            .bad_endpoints
            .iter()
            .filter(|e| !b.known_bad_endpoints.contains(*e))
            .cloned()
            .collect();

        // What the round actually CHANGED: compare the merged state with the
        // state it replaced, not with the raw probe. `apply_probe` unions
        // declared model names into `available_models` and keeps a `kind:`-pinned
        // family, neither of which ever equals the probe's own values, so a
        // state-vs-probe comparison would flag a change on every single round
        // and wake the dispatcher every 10 s for nothing.
        let was_api_type = b.api_type;
        let now_online = probe.is_online;
        let prev_available = b.available_models.clone();
        let prev_loaded = b.loaded_models.clone();
        crate::control::apply_probe(b, probe);
        let changed = b.is_online != now_online
            || b.api_type != was_api_type
            || b.available_models != prev_available
            || b.loaded_models != prev_loaded;

        if !was_online && now_online {
            info!("Backend {} status changed to: ONLINE", url);
        } else if was_online && !now_online {
            info!("Backend {} status changed to: OFFLINE", url);
        }
        if b.api_type != was_api_type {
            info!("Backend {} API type detected: {}", url, b.api_type.display());
        }
        for e in &newly_bad {
            info!(
                "Backend {} does not serve {} — remembered, will skip it until re-check",
                url, e
            );
        }

        any_changed |= changed;
    }

    // Wake the dispatcher if anything relevant to scheduling changed (model
    // finished loading, backend came online, etc.) so queued requests are
    // re-evaluated immediately.
    if any_changed {
        state.notify.notify_one();
    }
}

pub async fn run_worker(state: Arc<AppState>) {
    let client = state.client.clone();
    let mut current_idx = 0;

    // Background Health Check
    let health_state = state.clone();
    let health_client = client.clone();
    tokio::spawn(async move {
        let mut cycle: u32 = 0;
        loop {
            // Every ~1 min do a full re-probe of all endpoints so remembered
            // "bad" ones are re-verified (the backend may have been upgraded).
            let full_reprobe = cycle.is_multiple_of(6);
            cycle += 1;
            health_check_round(
                &health_state,
                &health_client,
                full_reprobe,
                crate::control::PROBE_TIMEOUT,
            )
            .await;
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });

    // Bound the per-user bookkeeping (see `prune_idle_users`).
    let prune_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(USER_PRUNE_INTERVAL).await;
            prune_idle_users(&prune_state, USER_RETENTION);
            prune_affinity(&prune_state);
            prune_conv_cache(&prune_state);
        }
    });

    loop {
        // Cache-aware hold-back bookkeeping for this selection round (see
        // `cold_hold` and the ordering block below).
        let mut cold_present = false;
        let mut first_hot_user: Option<String> = None;
        let mut first_reserved_user: Option<String> = None;
        let mut cold_users: Vec<String> = Vec::new();
        let selection_opt = {
            // Snapshot the model config once, before the queues lock, so the
            // per-task pin lookup below never takes model_config (lock order:
            // model_config -> queues -> backends).
            let configs = state.model_config.lock_or_recover().clone();
            let mut queues = state.queues.lock_or_recover();
            // Backends with a model control op (load/unload) in flight are treated as
            // busy: loading a new model can evict whatever is currently running.
            let control_busy: HashSet<usize> =
                state.control_ops.lock_or_recover().keys().copied().collect();
            // Model pins for every queued task, computed BEFORE taking the
            // backends lock (the model_config lock must not nest under it —
            // config reloads run concurrently). A pin restricts a model's
            // requests to the backends listed in appconf.yaml (strict).
            let task_pins: HashMap<(String, usize), Vec<String>> = queues
                .iter()
                .flat_map(|(user, q)| {
                    q.iter().enumerate().filter_map(|(pos, t)| match &t.requested_model {
                        Some(model) => {
                            model_pin_for(model, &configs).map(|pin| ((user.clone(), pos), pin))
                        }
                        None => None,
                    })
                })
                .collect();
            // Taken before the backends lock: the policy lock is a leaf.
            let policy = state.model_policy();
            let mut backends = state.backends.lock_or_recover();
            let mut last_idx = state
                .last_backend_idx
                .load(std::sync::atomic::Ordering::Relaxed);

            // 1. Pick a user and peek at their front task to know required API family
            let vip = state.vip_user.lock_or_recover().clone();
            let boost = state.boost_user.lock_or_recover().clone();
            let counter = state
                .global_counter
                .load(std::sync::atomic::Ordering::Relaxed);

            let mut active_users: Vec<String> = queues
                .iter()
                .filter(|(_, q)| !q.is_empty())
                .map(|(u, _)| u.clone())
                .collect();

            if active_users.is_empty() {
                None
            } else {
                {
                    // Least recent load first. One acquisition for the whole
                    // sort: the comparator used to lock and release the counts
                    // twice per comparison — O(n log n) acquisitions, nested
                    // inside the four locks already held here.
                    let now = std::time::Instant::now();
                    let scores = state.fair_share.lock_or_recover();
                    active_users.sort_by(|a, b| {
                        let a_load = scores.get(a).map_or(0.0, |s| s.value_at(now));
                        let b_load = scores.get(b).map_or(0.0, |s| s.value_at(now));
                        a_load
                            .partial_cmp(&b_load)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| a.cmp(b))
                    });
                }

                // Build candidate order: VIP first, then boost on every other
                // turn, then fair-share round-robin over the rest. The order is
                // a permutation of `active_users` held as indices — it used to
                // be built by cloning the user ids into two more Vec<String>
                // and de-duplicating with O(n) string comparisons.
                let mut order: Vec<usize> = Vec::with_capacity(active_users.len());
                let position_of = |name: &String| active_users.iter().position(|u| u == name);
                if let Some(i) = vip.as_ref().and_then(&position_of) {
                    order.push(i);
                }
                if counter.is_multiple_of(2)
                    && let Some(i) = boost.as_ref().and_then(&position_of)
                    && !order.contains(&i)
                {
                    order.push(i);
                }
                if current_idx >= active_users.len() {
                    current_idx = 0;
                }
                let rest: Vec<usize> = (0..active_users.len())
                    .filter(|i| !order.contains(i))
                    .collect();
                current_idx += 1;
                if !rest.is_empty() {
                    let start = current_idx % rest.len();
                    let rotated: Vec<usize> = rest[start..]
                        .iter()
                        .copied()
                        .chain(rest[..start].iter().copied())
                        .collect();
                    // Cache-aware hold-back: a conversation whose last
                    // prefill missed the cache of the backend that last
                    // served it lines up behind the recently served (likely
                    // still resident) ones, instead of forcing a context
                    // switch and a full re-prefill every round (see
                    // `cold_hold`). Where the signal is absent this is the
                    // plain fair order, unchanged.
                    let now = std::time::Instant::now();
                    let affinity = state.affinity.lock_or_recover();
                    let conv_cache = state.conv_cache.lock_or_recover();
                    let reserve = state.slot_reserve.lock_or_recover();
                    let is_reserved = |ui: usize| {
                        slot_reserved(
                            queues
                                .get(&active_users[ui])
                                .and_then(|q| q.front())
                                .and_then(|t| t.session_key.as_ref()),
                            &reserve,
                            now,
                        )
                    };
                    let is_cold = |ui: usize| {
                        queues
                            .get(&active_users[ui])
                            .and_then(|q| q.front())
                            .is_some_and(|t| {
                                cold_hold(
                                    t.session_key.as_ref(),
                                    t.cache_passes,
                                    &affinity,
                                    &conv_cache,
                                    &backends,
                                    now,
                                )
                            })
                    };
                    // Slot reservation first: a session that finished a turn
                    // within SLOT_RESERVE keeps the queue head for its own
                    // follow-up, so an agent can run a whole cycle without
                    // another session touching its backend mid-cycle.
                    let (reserved, rest_rot): (Vec<usize>, Vec<usize>) =
                        rotated.into_iter().partition(|&ui| is_reserved(ui));
                    let (hot, cold): (Vec<usize>, Vec<usize>) =
                        rest_rot.into_iter().partition(|&ui| !is_cold(ui));
                    cold_present = !cold.is_empty();
                    first_hot_user = reserved
                        .first()
                        .or(hot.first())
                        .map(|&ui| active_users[ui].clone());
                    first_reserved_user = if order.len() + reserved.len() + hot.len() + cold.len()
                        > 1
                    {
                        reserved.first().map(|&ui| active_users[ui].clone())
                    } else {
                        None
                    };
                    cold_users = cold.iter().map(|&ui| active_users[ui].clone()).collect();
                    order.extend(reserved);
                    order.extend(hot);
                    order.extend(cold);
                }

                let limits = state.model_limits.lock_or_recover();

                // Try users in candidate order. Within a user's queue, pick
                // the FIRST routable task (not just the front) so an
                // unroutable request — e.g. an Ollama-family call with every
                // Ollama backend offline — can't starve everything behind it.
                let mut selection: Option<(String, Task, usize, String, Option<String>, bool)> =
                    None;
                'users: for &ui in &order {
                    let user_id = &active_users[ui];
                    let queue_len = queues.get(user_id).map(|q| q.len()).unwrap_or(0);
                    for pos in 0..queue_len {
                        let task_ref = match queues.get(user_id).and_then(|q| q.get(pos)) {
                            Some(t) => t,
                            None => continue,
                        };
                        // Cloned before the task is removed below: `task_ref`
                        // borrows `queues` and cannot outlive the removal.
                        let pending_session_key = task_ref.session_key.clone();
                        let route_families =
                            families_for_route(&task_ref.method, &task_ref.path);
                        // Metadata reads are cheap and don't use the model
                        // runner: they ignore backend capacity and in-flight
                        // control ops, and are not accounted against either.
                        let metadata = is_metadata_path(&task_ref.path);
                        debug!(
                            "Request for user {}: pos={} path={} family={:?}",
                            user_id, pos, task_ref.path, route_families
                        );

                        // Backend pin for this task (pre-computed above, before
                        // the backends lock): Some(urls) restricts routing to
                        // exactly those backends; None = no restriction.
                        let pin = task_pins.get(&(user_id.clone(), pos)).cloned();

                        // Find eligible backends: online, has capacity (global per-backend
                        // cap + per-model limit), and support the required API + Model.
                        // `limits` is taken once per selection round, above —
                        // it used to be locked and dropped for every task examined.
                        let model_key =
                            task_ref.requested_model.as_deref().map(model_concurrency_key);
                        // Per-model limit on a given backend: the configured
                        // `max_concurrent_requests`, else 1 — or, on a backend that
                        // reports its slots, all of them (the server runs its own
                        // model in parallel on those slots).
                        let model_limit_on = |cap: u32, detected: bool| match &model_key {
                            Some(k) => limits
                                .get(k)
                                .copied()
                                .unwrap_or(if detected { cap } else { 1 }),
                            None => 0, // unused for model-less requests
                        };
                        let mut eligible_indices: Vec<usize> = backends.iter()
                            .enumerate()
                            .filter(|(i, b)| {
                                let online = b.is_online;
                                let (cap, detected) = state.backend_capacity(b);
                                let free = metadata
                                    || backend_has_capacity(
                                        b,
                                        model_key.as_deref(),
                                        model_limit_on(cap, detected),
                                        cap,
                                    );
                                let no_op = metadata || !control_busy.contains(i);
                                if !online || !free || !no_op {
                                    debug!(
                                        "Backend {} rejected: online={}, active={}/{}, control_op={}",
                                        b.url,
                                        online,
                                        b.active_requests,
                                        cap,
                                        !no_op
                                    );
                                }
                                online && free && no_op
                            })
                            .filter(|(_, b)| {
                                // The backend must speak the request's API dialect — ALWAYS,
                                // even when a specific model is requested. (Previously this was
                                // only checked in the no-model case, which let Ollama-family
                                // requests reach LM Studio backends that merely listed a
                                // matching model.) Unknown-type backends are allowed until the
                                // health check classifies them; families observed rejecting
                                // real traffic are excluded (see apply_family_learning).
                                if !family_compatible(b, route_families) {
                                    debug!(
                                        "Backend {} rejected: route families {:?} not supported by {:?}",
                                        b.url, route_families, b.api_type
                                    );
                                    return false;
                                }

                                // If a specific model is requested, backend MUST also have it.
                                match &task_ref.requested_model {
                                    Some(model) => {
                                        let has_model = model_routable(model, &b.available_models)
                                            && policy_allows(&policy, model, &b.url, &b.available_models);
                                        if !has_model {
                                            debug!(
                                                "Backend {} rejected: model '{}' not found. Available: {:?}",
                                                b.url, model, b.available_models
                                            );
                                        }
                                        has_model
                                    }
                                    None => true,
                                }
                            })
                            .map(|(i, _)| i)
                            .collect();

                        // Apply the pin filter upstream of tiering/selection: a
                        // pinned model may only be served by its listed backends.
                        if let Some(ref pins) = pin {
                            eligible_indices.retain(|&i| {
                                pins.iter().any(|p| pin_url_matches(p, &backends[i].url))
                            });
                        }

                        // Model-load-aware selection among eligible backends (see model_loaded_on):
                        //   tier 1 — the requested model is confirmed loaded on the backend, or the
                        //            backend doesn't report loaded state at all (e.g. vLLM: "available"
                        //            means "ready to serve");
                        //   tier 2 — the backend reports loaded state but has nothing loaded (cold
                        //            start; loading the requested model evicts nothing);
                        //   tier 3 — last resort: the backend has a DIFFERENT model loaded; on-demand
                        //            loading would evict it (slow, and may fail).
                        let eligible_indices = if let Some(ref model) = task_ref.requested_model {
                            let tier1: Vec<usize> = eligible_indices
                                .iter()
                                .cloned()
                                .filter(|&i| {
                                    let b = &backends[i];
                                    model_loaded_on(b, model) || !b.loaded_state_known
                                })
                                .collect();
                            if !tier1.is_empty() {
                                if tier1.len() < eligible_indices.len() {
                                    debug!(
                                        "model '{}' ready on {} of {} eligible backend(s) — routing to loaded backend(s)",
                                        model,
                                        tier1.len(),
                                        eligible_indices.len()
                                    );
                                }
                                tier1
                            } else {
                                let tier2: Vec<usize> = eligible_indices
                                    .iter()
                                    .cloned()
                                    .filter(|&i| {
                                        let b = &backends[i];
                                        b.loaded_state_known && b.loaded_models.is_empty()
                                    })
                                    .collect();
                                if !tier2.is_empty() {
                                    debug!(
                                        "no backend has model '{}' loaded; using cold backend(s) (nothing loaded to evict)",
                                        model
                                    );
                                    tier2
                                } else {
                                    warn!(
                                        "no backend has model '{}' loaded and every eligible backend has a different model loaded; falling back to on-demand load (may evict a loaded model)",
                                        model
                                    );
                                    eligible_indices
                                }
                            }
                        } else {
                            eligible_indices
                        };

                        if eligible_indices.is_empty() {
                            // Can this request EVER be served in the current known state? If
                            // some backend is family-compatible and (has the model / needs no
                            // model), it may just be busy or loading — keep waiting. Otherwise
                            // nothing will ever pick it up: warn once, then fail fast with 503
                            // after stuck_timeout instead of letting the client hang forever.
                            // A disabled backend never comes back by itself.
                            let satisfiable = backends.iter().any(|b| {
                                !b.disabled
                                    && family_compatible(b, route_families)
                                    && match &task_ref.requested_model {
                                        // A pinned model is only satisfiable by a backend
                                        // that also passes the pin filter — strict pinning
                                        // fails with 503 instead of routing elsewhere.
                                        Some(model) => {
                                            model_routable(model, &b.available_models)
                                                && policy_allows(&policy, model, &b.url, &b.available_models)
                                                && pin.as_ref().is_none_or(|pins| {
                                                    pins.iter()
                                                        .any(|p| pin_url_matches(p, &b.url))
                                                })
                                        }
                                        None => true,
                                    }
                            });

                            if !satisfiable
                                && task_ref.queued_at.elapsed() >= state.stuck_timeout()
                            {
                                let dropped = queues
                                    .get_mut(user_id)
                                    .unwrap()
                                    .remove(pos)
                                    .unwrap();
                                release_queued_bytes(&state, dropped.body.len() as u64);
                                state
                                    .global_counter
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let waited = dropped.queued_at.elapsed().as_secs();
                                // Pinned models get a distinct, user-diagnosable message.
                                let (log_info, error_msg) = match &pin {
                                    Some(_) => {
                                        let model =
                                            dropped.requested_model.clone().unwrap_or_default();
                                        (
                                            format!(
                                                "503 no pinned backend can serve model '{}' (waited {}s)",
                                                model, waited
                                            ),
                                            format!(
                                                "no configured backend available for model '{}'",
                                                model
                                            ),
                                        )
                                    }
                                    None => (
                                        format!("503 no backend can serve (waited {}s)", waited),
                                        "no backend available to serve this request".to_string(),
                                    ),
                                };
                                let mut dropped_counts = state.dropped_counts.lock_or_recover();
                                *dropped_counts.entry(user_id.clone()).or_insert(0) += 1;
                                drop(dropped_counts);
                                let model =
                                    model_label_in(&backends, dropped.requested_model.as_deref());
                                state.metrics.inc(
                                    &crate::metrics::REQUESTS,
                                    &[user_id, &model, route_template(&dropped.path), "timeout"],
                                    1.0,
                                );
                                state.stats.record(crate::stats::Finished {
                                    at_ms: crate::stats::now_ms(),
                                    client: user_id.clone(),
                                    model,
                                    backend: None,
                                    outcome: "timeout",
                                    session: dropped.session_key.as_ref().map(SessionKey::tag),
                                    queue_wait: Some(dropped.queued_at.elapsed().as_secs_f64()),
                                    timing: Default::default(),
                                });
                                state.log_event(LogEvent {
                                    at: std::time::SystemTime::now(),
                                    dir: "OUT",
                                    user: user_id.clone(),
                                    model: dropped.requested_model.clone(),
                                    backend: None,
                                    info: log_info,
                                    content: None,
                                });
                                let responder = dropped.responder;
                                tokio::spawn(async move {
                                    let body =
                                        serde_json::json!({ "error": error_msg }).to_string();
                                    let mut headers = HeaderMap::new();
                                    headers.insert(
                                        axum::http::header::CONTENT_TYPE,
                                        axum::http::HeaderValue::from_static("application/json"),
                                    );
                                    let _ = responder
                                        .send(ResponsePart::Status(
                                            StatusCode::SERVICE_UNAVAILABLE,
                                            headers,
                                        ))
                                        .await;
                                    let _ = responder
                                        .send(ResponsePart::Chunk(Bytes::from(body)))
                                        .await;
                                });
                                continue 'users;
                            }

                            if !task_ref.stuck_warned {
                                let msg_prefix = if satisfiable {
                                    match &pin {
                                        // Satisfiable: some pinned backend may still serve
                                        // the request, so there is no 503 to promise — we
                                        // are merely waiting for it.
                                        Some(pins) => format!(
                                            "model '{}' is pinned to backend(s) {} which are not currently eligible; waiting for a pinned backend to become eligible",
                                            task_ref.requested_model.as_deref().unwrap_or("-"),
                                            pins.join(", ")
                                        ),
                                        None => "No backend free".to_string(),
                                    }
                                } else {
                                    match &pin {
                                        Some(pins) => format!(
                                            "no backend can serve model '{}' (pinned to: {}); will fail with 503 after {}s",
                                            task_ref.requested_model.as_deref().unwrap_or("-"),
                                            pins.join(", "),
                                            state.stuck_timeout().as_secs()
                                        ),
                                        None => format!(
                                            "No backend can serve; will fail with 503 after {}s",
                                            state.stuck_timeout().as_secs()
                                        ),
                                    }
                                };
                                warn!(
                                    "{} for {} {} (model: {}, family: {:?}) for user {}",
                                    msg_prefix,
                                    task_ref.method,
                                    task_ref.path,
                                    task_ref.requested_model.as_deref().unwrap_or("-"),
                                    route_families,
                                    user_id
                                );
                                if let Some(task_mut) =
                                    queues.get_mut(user_id).and_then(|q| q.get_mut(pos))
                                {
                                    task_mut.stuck_warned = true;
                                }
                            }
                            continue;
                        }

                        let task = queues.get_mut(user_id).unwrap().remove(pos).unwrap();
                        // Observe this follow-up's client turnaround to adapt
                        // the session's reservation window and chain (see
                        // [`note_follow_up`]).
                        if let Some(key) = task.session_key.as_ref() {
                            note_follow_up(state.as_ref(), key, task.queued_at);
                        }
                        // A cache-hot turn went ahead of the held
                        // conversations: charge each of them a yield, so
                        // being held back stays bounded (see `cold_hold`).
                        if !cold_users.is_empty() && !cold_users.contains(user_id) {
                            for cu in &cold_users {
                                if let Some(front) =
                                    queues.get_mut(cu).and_then(|q| q.front_mut())
                                {
                                    front.cache_passes = front.cache_passes.saturating_add(1);
                                }
                            }
                        }
                        release_queued_bytes(&state, task.body.len() as u64);
                        state
                            .global_counter
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Charge the user for the work now, not on success:
                        // a request that fails or is abandoned still consumed
                        // a backend, and must not buy priority for the next.
                        {
                            let now = std::time::Instant::now();
                            state
                                .fair_share
                                .lock_or_recover()
                                .entry(user_id.clone())
                                .or_insert_with(|| FairShare::new(now))
                                .charge(now);
                        }

                        // Round-Robin among eligible backends with min connections
                        let min_conns = eligible_indices
                            .iter()
                            .map(|&i| backends[i].active_requests)
                            .min()
                            .unwrap();
                        // Sticky routing: keep a conversation on the backend that
                        // served its previous turn when that backend can take it
                        // at reasonable load. See [`pick_sticky_backend`].
                        let sticky = pending_session_key.as_ref().and_then(|key| {
                            pick_sticky_backend(
                                state.as_ref(),
                                key,
                                &eligible_indices,
                                &backends,
                                min_conns,
                            )
                        });
                        let candidates_backends: Vec<usize> = eligible_indices
                            .iter()
                            .cloned()
                            .filter(|&i| backends[i].active_requests == min_conns)
                            .collect();
                        let candidate_pos = candidates_backends
                            .iter()
                            .position(|&i| i > last_idx)
                            .unwrap_or(0);
                        let selected_backend_idx =
                            sticky.unwrap_or(candidates_backends[candidate_pos]);

                        last_idx = selected_backend_idx;
                        state
                            .last_backend_idx
                            .store(last_idx, std::sync::atomic::Ordering::Relaxed);
                        if !metadata {
                            backends[selected_backend_idx].active_requests += 1;
                            if let Some(ref m) = task.requested_model {
                                let k = model_concurrency_key(m);
                                *backends[selected_backend_idx]
                                    .active_by_model
                                    .entry(k)
                                    .or_insert(0) += 1;
                            }
                            backends[selected_backend_idx].current_model =
                                task.requested_model.clone();
                        }

                        selection = Some((
                            user_id.clone(),
                            task,
                            selected_backend_idx,
                            backends[selected_backend_idx].url.clone(),
                            backends[selected_backend_idx].token.clone(),
                            metadata,
                        ));
                        if let Some(key) = pending_session_key {
                            record_affinity(
                                state.as_ref(),
                                &key,
                                &backends[selected_backend_idx].url,
                            );
                        }
                        break 'users;
                    }
                }

                selection
            }
        };

        match selection_opt {
            Some((user_id, task, backend_idx, backend_url, backend_token, metadata)) => {
                // A cache-cold user waited while this cache-hot dispatch ran
                // ahead of it (see `cold_hold`); its release is the same wait
                // visible as queue time in the activity view.
                if cold_present && first_hot_user.as_deref() == Some(user_id.as_str()) {
                    state
                        .metrics
                        .inc(&crate::metrics::CACHE_SCHEDULING, &["held_back"], 1.0);
                }
                if first_reserved_user.as_deref() == Some(user_id.as_str()) {
                    state
                        .metrics
                        .inc(&crate::metrics::CACHE_SCHEDULING, &["reserved_first"], 1.0);
                }
                let state_clone = state.clone();
                let client_clone = client.clone();
                let url = format!("{}{}", backend_url, task.path);

                tokio::spawn(async move {
                    let log_model = task.requested_model.clone();
                    let route_families = families_for_route(&task.method, &task.path);
                    // Metric labels, fixed for the request's lifetime.
                    let m_model = state_clone.model_label(task.requested_model.as_deref());
                    let m_endpoint = route_template(&task.path);
                    let m_backend = crate::metrics::backend_label(&backend_url);
                    let m_key = task.key_label.clone();
                    let metrics = &state_clone.metrics;
                    // Tokens go to the per-request counter and, for requests
                    // sent with a client key, the per-key one.
                    let count_tokens = |kind: &str, n: u64| {
                        metrics.inc(
                            &crate::metrics::TOKENS,
                            &[&user_id, &m_model, &m_backend, kind],
                            n as f64,
                        );
                        if let Some(key) = &m_key {
                            metrics.inc(&crate::metrics::KEY_TOKENS, &[&user_id, key, kind], n as f64);
                        }
                    };
                    let queue_wait = task.queued_at.elapsed().as_secs_f64();
                    let session_tag = task.session_key.as_ref().map(SessionKey::tag);
                    // Counted for Prometheus and kept for the Overview.
                    let outcome = |o: &'static str, timing: crate::stats::Timing| {
                        metrics.inc(
                            &crate::metrics::REQUESTS,
                            &[&user_id, &m_model, m_endpoint, o],
                            1.0,
                        );
                        state_clone.stats.record(crate::stats::Finished {
                            at_ms: crate::stats::now_ms(),
                            client: user_id.clone(),
                            model: m_model.clone(),
                            backend: Some(m_backend.clone()),
                            outcome: o,
                            session: session_tag.clone(),
                            queue_wait: Some(queue_wait),
                            timing,
                        });
                    };
                    metrics.observe(&crate::metrics::QUEUE_WAIT, &[&user_id, &m_model], queue_wait);
                    // Updated when the response ends; start them at zero so a
                    // long request's tokens are not lost (see `Metrics::touch`).
                    for kind in ["prompt", "completion"] {
                        metrics.touch(&crate::metrics::TOKENS, &[&user_id, &m_model, &m_backend, kind]);
                        if let Some(key) = &m_key {
                            metrics.touch(&crate::metrics::KEY_TOKENS, &[&user_id, key, kind]);
                        }
                    }
                    metrics.touch(&crate::metrics::RESPONSE_BYTES, &[&user_id, &m_model, &m_backend]);
                    metrics.touch_histogram(&crate::metrics::TTFB, &[&m_model, &m_backend]);
                    metrics.touch_histogram(&crate::metrics::TTFT, &[&user_id, &m_model, &m_backend]);
                    metrics.touch_histogram(&crate::metrics::GEN_SPEED, &[&m_model, &m_backend]);
                    for def in [
                        &crate::metrics::GEN_SECONDS,
                        &crate::metrics::GEN_TOKENS,
                        &crate::metrics::PROMPT_SECONDS,
                        &crate::metrics::PROMPT_EVAL_TOKENS,
                    ] {
                        metrics.touch(def, &[&user_id, &m_model, &m_backend]);
                    }
                    metrics.touch_histogram(&crate::metrics::DURATION, &[&m_model, &m_backend]);
                    let log_out = |info: String,
                                   backend: Option<String>,
                                   content: Option<Arc<String>>| {
                        state_clone.log_event(LogEvent {
                            at: std::time::SystemTime::now(),
                            dir: "OUT",
                            user: user_id.clone(),
                            model: log_model.clone(),
                            backend,
                            info,
                            content,
                        });
                    };

                    // Set only when a response was delivered to the client in
                    // full; `processed_count` used to be bumped on every exit
                    // path, so the TUI counted blocked, abandoned and errored
                    // requests as throughput.
                    let mut delivered = false;

                    let is_blocked = {
                        let user_ips = state_clone.user_ips.lock_or_recover();
                        let blocked_ips = state_clone.blocked_ips.lock_or_recover();
                        let blocked_users = state_clone.blocked_users.lock_or_recover();
                        blocked_users.contains(&user_id)
                            || user_ips
                                .get(&user_id)
                                .map(|ip| blocked_ips.contains(ip))
                                .unwrap_or(false)
                    } || state_clone.clients.lock_or_recover().is_disabled(&user_id);

                    if is_blocked || task.responder.is_closed() {
                        let mut dropped = state_clone.dropped_counts.lock_or_recover();
                        *dropped.entry(user_id.clone()).or_insert(0) += 1;
                        log_out("dropped (blocked)".into(), Some(backend_url.clone()), None);
                        outcome("blocked", Default::default());
                    } else {
                        {
                            let mut processing = state_clone.processing_counts.lock_or_recover();
                            *processing.entry(user_id.clone()).or_insert(0) += 1;
                        }

                        // Response size + bounded content prefix for the
                        // request log (appends stop at log_content_limit, but
                        // total_bytes keeps counting).
                        let mut total_bytes: u64 = 0;
                        let mut content_acc: Vec<u8> = Vec::new();
                        // The end of the body, where token usage is reported.
                        let mut tail: Vec<u8> = Vec::new();
                        let started = std::time::Instant::now();
                        // `task.method`/`task.path` are moved into the request
                        // builder below; capture strings for log records.
                        let method_str = task.method.to_string();
                        let path_str = task.path.clone();

                        // Shown as running until this block ends, on any path.
                        // The real prompt count arrives with the response usage;
                        // this rough estimate is what capacity can size by now.
                        let est_prompt_tokens = (task.body.len() / 4) as u64;
                        let run = state_clone.runs.start(
                            &user_id,
                            m_key.as_deref().unwrap_or("none"),
                            &m_model,
                            &m_backend,
                            queue_wait,
                            est_prompt_tokens,
                            session_tag.as_deref(),
                        );
                        // Completion tokens already counted from the stream.
                        let mut live_tokens: u64 = 0;

                        // Attach this backend's configured auth token unless the
                        // client already presented its own credentials.
                        let mut headers = task.headers.clone();
                        attach_backend_token(&mut headers, backend_token.as_deref());

                        let res_fut = client_clone
                            .request(task.method, &url)
                            .timeout(std::time::Duration::from_secs(
                                state_clone.timeout.load(std::sync::atomic::Ordering::Relaxed),
                            ))
                            .headers(headers)
                            .body(task.body)
                            .send();

                        match res_fut.await {
                            Ok(response) => {
                                let status = response.status();
                                let mut headers = response.headers().clone();
                                headers.remove(axum::http::header::TRANSFER_ENCODING);
                                headers.remove(axum::http::header::CONTENT_LENGTH);
                                let resp_content_type = headers
                                    .get(axum::http::header::CONTENT_TYPE)
                                    .and_then(|v| v.to_str().ok())
                                    .map(str::to_string);

                                if task
                                    .responder
                                    .send(ResponsePart::Status(status, headers))
                                    .await
                                    .is_ok()
                                {
                                    let mut stream = response.bytes_stream();
                                    let mut client_disconnected = false;
                                    // Keep a short prefix of the first body chunk so we can
                                    // tell "endpoint rejected" error bodies apart from normal
                                    // application errors (learning hook below).
                                    let mut first_prefix: Option<String> = None;
                                    // Streams (SSE / NDJSON) show their first token as it
                                    // is generated; a plain JSON body arrives all at once.
                                    let streaming = resp_content_type.as_deref().is_some_and(|c| {
                                        c.starts_with("text/event-stream") || c.contains("ndjson")
                                    });
                                    let mut first_token: Option<std::time::Instant> = None;
                                    while let Some(chunk_res) = stream.next().await {
                                        match chunk_res {
                                            Ok(chunk) => {
                                                if first_prefix.is_none() {
                                                    metrics.observe(
                                                        &crate::metrics::TTFB,
                                                        &[&m_model, &m_backend],
                                                        started.elapsed().as_secs_f64(),
                                                    );
                                                    run.update(|r| {
                                                        r.first_byte = Some(std::time::Instant::now())
                                                    });
                                                    let n = chunk.len().min(512);
                                                    first_prefix = Some(
                                                        String::from_utf8_lossy(&chunk[..n])
                                                            .into_owned(),
                                                    );
                                                }
                                                let n = chunk.len();
                                                total_bytes += n as u64;
                                                crate::metrics::push_tail(&mut tail, &chunk);
                                                // Count generated tokens as they stream, so
                                                // runs in progress show up live.
                                                let est = crate::metrics::estimate_stream_tokens(&chunk);
                                                if est > 0 && streaming && first_token.is_none() {
                                                    let now = std::time::Instant::now();
                                                    first_token = Some(now);
                                                    metrics.observe(
                                                        &crate::metrics::TTFT,
                                                        &[&user_id, &m_model, &m_backend],
                                                        (now - started).as_secs_f64(),
                                                    );
                                                    run.update(|r| r.first_token = Some(now));
                                                }
                                                if est > 0 {
                                                    live_tokens += est;
                                                    count_tokens("completion", est);
                                                    run.update(|r| r.tokens += est);
                                                }
                                                if content_acc.len() < state_clone.log_content_limit.load(std::sync::atomic::Ordering::Relaxed)
                                                {
                                                    let room = state_clone.log_content_limit.load(std::sync::atomic::Ordering::Relaxed)
                                                        - content_acc.len();
                                                    content_acc
                                                        .extend_from_slice(&chunk[..n.min(room)]);
                                                }
                                                if task
                                                    .responder
                                                    .send(ResponsePart::Chunk(chunk))
                                                    .await
                                                    .is_err()
                                                {
                                                    client_disconnected = true;
                                                    break;
                                                }
                                            }
                                            Err(_) => break,
                                        }
                                    }

                                    // Learn from the response: endpoint rejections (405 /
                                    // "Unexpected endpoint or method") mark this backend as
                                    // incompatible with the request's API family after two
                                    // strikes; any other response clears that memory.
                                    let prefix = first_prefix.unwrap_or_default();
                                    apply_family_learning(
                                        &state_clone,
                                        backend_idx,
                                        route_families,
                                        is_endpoint_rejection(status, &prefix),
                                    );

                                    metrics.inc(
                                        &crate::metrics::BACKEND_RESPONSES,
                                        &[&user_id, &m_model, &m_backend, status.as_str()],
                                        1.0,
                                    );
                                    metrics.inc(
                                        &crate::metrics::RESPONSE_BYTES,
                                        &[&user_id, &m_model, &m_backend],
                                        total_bytes as f64,
                                    );
                                    metrics.observe(
                                        &crate::metrics::DURATION,
                                        &[&m_model, &m_backend],
                                        started.elapsed().as_secs_f64(),
                                    );
                                    // The exact usage replaces the stream estimate: only
                                    // the difference is added (a counter cannot go down).
                                    let usage = crate::metrics::parse_usage(&tail);
                                    // How much of a served conversation's
                                    // prompt the backend skipped from its KV
                                    // cache — the scheduler's view of who is
                                    // cache-hot where (see `cold_hold`).
                                    if status.is_success()
                                        && !client_disconnected
                                        && let Some(key) = task.session_key.as_ref()
                                    {
                                        // The follow-up (the client's next tool
                                        // call) keeps queue priority while the
                                        // session visibly stays active.
                                        reserve_slot(&state_clone, key);
                                        if let Some(hit) =
                                            usage.and_then(|u| cache_hit_pct(u.prompt, u.prompt_eval))
                                        {
                                            // What the next turn must re-feed
                                            // if the cache keeps nothing of
                                            // this one: what was evaluated
                                            // plus what was decoded.
                                            let inc = usage
                                                .and_then(|u| u.prompt_eval)
                                                .map(|pe| pe.saturating_add(usage.map_or(0, |u| u.completion)));
                                            record_conv_cache(
                                                &state_clone, key, &backend_url, hit, inc,
                                            );
                                        }
                                    }
                                    if let Some(u) = usage {
                                        count_tokens("prompt", u.prompt);
                                        count_tokens(
                                            "completion",
                                            u.completion.saturating_sub(live_tokens),
                                        );
                                    }
                                    let mut timing = crate::stats::Timing {
                                        status: Some(status.as_u16()),
                                        duration: Some(started.elapsed().as_secs_f64()),
                                        ttft: first_token.map(|t| (t - started).as_secs_f64()),
                                        prompt_tokens: usage.map_or(0, |u| u.prompt),
                                        completion_tokens: usage.map_or(live_tokens, |u| u.completion),
                                        ..Default::default()
                                    };
                                    if status.is_success() && !client_disconnected {
                                        record_speed(
                                            metrics,
                                            [&user_id, &m_model, &m_backend],
                                            usage,
                                            first_token,
                                            live_tokens,
                                            &mut timing,
                                        );
                                    }
                                    outcome(
                                        if client_disconnected { "client_gone" } else { "ok" },
                                        timing,
                                    );

                                    if !client_disconnected {
                                        delivered = true;
                                        let mut counts =
                                            state_clone.processed_counts.lock_or_recover();
                                        *counts.entry(user_id.clone()).or_insert(0) += 1;
                                        drop(counts);
                                        let content_str = Arc::new(crate::reqlog::truncate_utf8(
                                            &content_acc,
                                            state_clone.log_content_limit.load(std::sync::atomic::Ordering::Relaxed),
                                        ));
                                        state_clone.reqlog.log(crate::reqlog::ReqRecord {
                                            ts: crate::reqlog::now_unix_millis(),
                                            dir: "OUT",
                                            user: task.user.clone(),
                                            model: log_model.clone(),
                                            backend: Some(backend_url.clone()),
                                            method: method_str.clone(),
                                            path: path_str.clone(),
                                            status: Some(status.as_u16()),
                                            bytes: Some(total_bytes),
                                            content_type: resp_content_type,
                                            content: Some(content_str.clone()),
                                        });
                                        log_out(
                                            format!(
                                                "{} {} -> {} resp={}B",
                                                method_str, path_str, status.as_u16(), total_bytes
                                            ),
                                            Some(backend_url.clone()),
                                            Some(content_str),
                                        );
                                    } else {
                                        let mut dropped =
                                            state_clone.dropped_counts.lock_or_recover();
                                        *dropped.entry(user_id.clone()).or_insert(0) += 1;
                                        log_out(
                                            "dropped (client gone)".into(),
                                            Some(backend_url.clone()),
                                            None,
                                        );
                                    }
                                } else {
                                    outcome("client_gone", Default::default());
                                }
                            }
                            Err(e) => {
                                let err_msg = Arc::new(format!("upstream error: {}", e));
                                state_clone.reqlog.log(crate::reqlog::ReqRecord {
                                    ts: crate::reqlog::now_unix_millis(),
                                    dir: "OUT",
                                    user: task.user.clone(),
                                    model: log_model.clone(),
                                    backend: Some(backend_url.clone()),
                                    method: method_str,
                                    path: path_str,
                                    status: None,
                                    bytes: Some(total_bytes),
                                    content_type: None,
                                    content: Some(err_msg.clone()),
                                });
                                let _ = task.responder.send(ResponsePart::Error(e)).await;
                                let mut dropped = state_clone.dropped_counts.lock_or_recover();
                                *dropped.entry(user_id.clone()).or_insert(0) += 1;
                                log_out(
                                    "dropped (backend error)".into(),
                                    Some(backend_url.clone()),
                                    Some(err_msg),
                                );
                                outcome(
                                    "backend_error",
                                    crate::stats::Timing {
                                        duration: Some(started.elapsed().as_secs_f64()),
                                        ..Default::default()
                                    },
                                );
                            }
                        }

                        {
                            let mut processing = state_clone.processing_counts.lock_or_recover();
                            if let Some(count) = processing.get_mut(&user_id) {
                                *count = count.saturating_sub(1);
                            }
                        }
                    }

                    {
                        let mut backends = state_clone.backends.lock_or_recover();
                        let b = &mut backends[backend_idx];
                        // Metadata reads were never accounted at dispatch.
                        if !metadata {
                            b.active_requests = b.active_requests.saturating_sub(1);
                            if let Some(ref m) = task.requested_model {
                                let k = model_concurrency_key(m);
                                if let Some(c) = b.active_by_model.get_mut(&k) {
                                    *c = c.saturating_sub(1);
                                    if *c == 0 {
                                        b.active_by_model.remove(&k);
                                    }
                                }
                            }
                            // Nothing of ours is running here any more, so the
                            // backend has no current model. It used to keep
                            // showing the last model it ever touched.
                            if b.active_requests == 0 {
                                b.current_model = None;
                            }
                        }
                        if delivered {
                            b.processed_count += 1;
                        }
                    }
                    state_clone.backend_freed.notify_one();
                });
            }
            None => {
                // Tick at least once per second so stuck-timeout drops are
                // enforced even when no new events arrive.
                tokio::select! {
                    _ = state.notify.notified() => {},
                    _ = state.backend_freed.notified() => {},
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
                }
            }
        }
    }
}

pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    method: Method,
    mut headers: HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    body: Bytes,
) -> impl IntoResponse {
    // Olla-style paths (`/olla/openai/v1/...`, `/olla/proxy/api/...`) are the
    // plain routes under a prefix, so clients set up for Olla only change port.
    let path = strip_olla_prefix(uri.path()).to_string();
    // Only known routes behind the prefix: the catch-all Olla routes must not
    // open what the explicit route list (allow_all_routes off) keeps closed.
    if path.len() != uri.path().len() && route_template(&path) == "other" {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }
    let ip = addr.ip();
    let body = state.apply_model_alias(body);
    // A client key decides the queue identity and beats `X-User-ID`, so a key
    // holder cannot hop queues. No key, or one no client owns, falls back to
    // `X-User-ID` as before.
    let client = crate::auth::extract_api_key(&headers)
        .and_then(|key| state.clients.lock_or_recover().resolve(&key));
    let user_id = match &client {
        Some(c) => c.client.clone(),
        None => match headers.get("X-User-ID").and_then(|h| h.to_str().ok()) {
            Some(claimed) => state.unkeyed_identity(claimed),
            None if state.ip_identity.load(std::sync::atomic::Ordering::Relaxed) => {
                format!("ip-{ip}")
            }
            None => state.unkeyed_identity("anonymous"),
        },
    };
    // Metrics for requests refused before they are queued.
    let endpoint = route_template(&path);
    let refused = |outcome: &str| {
        let model = state.model_label(extract_requested_model(&body).as_deref());
        state.metrics.inc(
            &crate::metrics::REQUESTS,
            &[&user_id, &model, endpoint, outcome],
            1.0,
        );
    };
    if let Some(c) = &client {
        state.metrics.inc(
            &crate::metrics::KEY_REQUESTS,
            &[&c.client, &c.key_id, &c.key_label],
            1.0,
        );
        if c.disabled {
            warn!("Request with a key of disabled client: {} (IP: {})", user_id, ip);
            refused("blocked");
            return (StatusCode::FORBIDDEN, "Client disabled").into_response();
        }
        // The key is a credential for ollamaMQ, not for the backend: drop it
        // so it never leaves the proxy and the backend's own token applies.
        headers.remove(axum::http::header::AUTHORIZATION);
        headers.remove("x-api-key");
    }

    if state.is_ip_blocked(&ip) {
        warn!("Blocked request from IP: {} for user: {}", ip, user_id);
        refused("blocked");
        return (StatusCode::FORBIDDEN, "IP blocked").into_response();
    }

    if state.is_user_blocked(&user_id) {
        warn!("Blocked request from user: {} (IP: {})", user_id, ip);
        refused("blocked");
        return (StatusCode::FORBIDDEN, "User blocked").into_response();
    }

    {
        let mut ips = state.user_ips.lock_or_recover();
        ips.insert(user_id.clone(), ip);
    }

    // Mark the user as seen (without charging them) so idle-user pruning has
    // an accurate last-seen even for requests that never get dispatched.
    {
        let now = std::time::Instant::now();
        state
            .fair_share
            .lock_or_recover()
            .entry(user_id.clone())
            .or_insert_with(|| FairShare::new(now))
            .touch(now);
    }

    // Model listings are answered from every backend at once instead of being
    // routed to one of them (see `aggregate_model_list`). If nothing can be
    // merged, fall through to the normal queued path.
    if method == Method::GET
        && model_list_shapes(&path).is_some()
        && let Some(response) = aggregate_model_list(&state, &path).await
    {
        let ts = crate::reqlog::now_unix_millis();
        for (dir, info) in [
            ("IN", format!("GET {} body=0B", path)),
            ("OUT", format!("GET {} -> 200 aggregated", path)),
        ] {
            state.reqlog.log(crate::reqlog::ReqRecord {
                ts,
                dir,
                user: addr.to_string(),
                model: None,
                backend: Some("<all backends>".to_string()),
                method: "GET".to_string(),
                path: path.clone(),
                status: (dir == "OUT").then_some(200),
                bytes: Some(0),
                content_type: None,
                content: None,
            });
            state.log_event(LogEvent {
                at: std::time::SystemTime::now(),
                dir,
                user: user_id.clone(),
                model: None,
                backend: Some("<all backends>".to_string()),
                info,
                content: None,
            });
        }
        state.metrics.inc(
            &crate::metrics::REQUESTS,
            &[&user_id, "none", endpoint, "ok"],
            1.0,
        );
        return response;
    }

    let (tx, rx) = mpsc::channel(32);
    let mut task_headers = headers.clone();
    task_headers.remove(axum::http::header::HOST);
    // The body may have been rewritten (model alias); the HTTP client sets
    // the length of the body actually sent.
    task_headers.remove(axum::http::header::CONTENT_LENGTH);
    task_headers.remove(axum::http::header::TRANSFER_ENCODING);

    let requested_model = extract_requested_model(&body);

    // A hidden model (see `crate::model_policy`) is answered 404 at once
    // instead of waiting out stuck_timeout. `/v1/models/{id}` names its model
    // in the path rather than the body.
    let policy_model = requested_model.clone().or_else(|| {
        path.strip_prefix("/v1/models/")
            .filter(|m| !m.is_empty())
            .map(str::to_string)
    });
    if let Some(model) = policy_model.as_deref()
        && state.model_blocked(model)
    {
        info!("Request for hidden model '{}' from {} refused", model, user_id);
        state.log_event(LogEvent {
            at: std::time::SystemTime::now(),
            dir: "OUT",
            user: user_id.clone(),
            model: Some(model.to_string()),
            backend: None,
            info: "404 model hidden".to_string(),
            content: None,
        });
        let label = state.model_label(Some(model));
        state.metrics.inc(
            &crate::metrics::REQUESTS,
            &[&user_id, &label, endpoint, "hidden_model"],
            1.0,
        );
        return model_not_found(&path, model);
    }

    let task_method = method.as_str().to_string();
    // Sticky routing needs to know which conversation this request belongs to,
    // and that can only be answered while the body is here (see
    // [`derive_session_key`]).
    let session_key = derive_session_key(
        StickyMode::from_u8(
            state
                .sticky_mode
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
        &path,
        &headers,
        &body,
        requested_model.as_deref(),
        &user_id,
    );
    // Before the queues lock below: this takes the backends lock.
    let model_label = state.model_label(requested_model.as_deref());
    // Kept copy of the conversation tag: `session_key` moves into the task.
    let session_tag = session_key.as_ref().map(SessionKey::tag);
    // Request-log content is captured before `body` moves into the Task.
    let body_len = body.len() as u64;
    let content = Arc::new(crate::reqlog::truncate_utf8(&body, state.log_content_limit.load(std::sync::atomic::Ordering::Relaxed)));
    let task = Task {
        path: path.clone(),
        method,
        user: addr.to_string(),
        headers: task_headers,
        responder: tx,
        body,
        requested_model: requested_model.clone(),
        stuck_warned: false,
        cache_passes: 0,
        queued_at: std::time::Instant::now(),
        key_label: client.as_ref().map(|c| {
            if c.key_label.is_empty() {
                c.key_id.clone()
            } else {
                c.key_label.clone()
            }
        }),
        session_key,
    };

    {
        let mut queues = state.queues.lock_or_recover();
        let queue = queues.entry(user_id.clone()).or_default();
        // A per-user ceiling: queued tasks hold their whole request body, so
        // an unbounded queue is an unbounded memory commitment.
        if queue.len() >= MAX_QUEUED_PER_USER {
            drop(queues);
            warn!(
                "User {} already has {} requests queued; refusing this one",
                user_id, MAX_QUEUED_PER_USER
            );
            *state
                .dropped_counts
                .lock_or_recover()
                .entry(user_id.clone())
                .or_insert(0) += 1;
            state.log_event(LogEvent {
                at: std::time::SystemTime::now(),
                dir: "OUT",
                user: user_id.clone(),
                model: requested_model.clone(),
                backend: None,
                info: format!("429 queue full ({} waiting)", MAX_QUEUED_PER_USER),
                content: None,
            });
            state.metrics.inc(
                &crate::metrics::REQUESTS,
                &[&user_id, &model_label, endpoint, "queue_full"],
                1.0,
            );
            state.stats.record(crate::stats::Finished {
                at_ms: crate::stats::now_ms(),
                client: user_id.clone(),
                model: model_label.clone(),
                backend: None,
                outcome: "queue_full",
                session: session_tag.clone(),
                queue_wait: None,
                timing: Default::default(),
            });
            return (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(serde_json::json!({
                    "error": "too many queued requests for this user"
                })),
            )
                .into_response();
        }
        // Bodies are held for as long as the request waits, so the request
        // count alone did not bound the memory: 100 queued requests at the
        // maximum body size is gigabytes. A request is always admitted when
        // the queues are empty, so an oversized body can never lock the proxy
        // out of making progress.
        let queued_now = state
            .queued_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        let max_queued = state.max_queued_bytes.load(std::sync::atomic::Ordering::Relaxed);
        if queued_now > 0 && queued_now + body_len > max_queued {
            drop(queues);
            warn!(
                "queued bodies at {} B (limit {}); refusing a {} B request from {}",
                queued_now, max_queued, body_len, user_id
            );
            *state
                .dropped_counts
                .lock_or_recover()
                .entry(user_id.clone())
                .or_insert(0) += 1;
            state.log_event(LogEvent {
                at: std::time::SystemTime::now(),
                dir: "OUT",
                user: user_id.clone(),
                model: requested_model.clone(),
                backend: None,
                info: format!("503 queue memory full ({} B waiting)", queued_now),
                content: None,
            });
            state.metrics.inc(
                &crate::metrics::REQUESTS,
                &[&user_id, &model_label, endpoint, "overloaded"],
                1.0,
            );
            state.stats.record(crate::stats::Finished {
                at_ms: crate::stats::now_ms(),
                client: user_id.clone(),
                model: model_label.clone(),
                backend: None,
                outcome: "overloaded",
                session: session_tag.clone(),
                queue_wait: None,
                timing: Default::default(),
            });
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "proxy queue is full; retry shortly"
                })),
            )
                .into_response();
        }
        queue.push_back(task);
        state
            .queued_bytes
            .fetch_add(body_len, std::sync::atomic::Ordering::Relaxed);
    }
    state.metrics.inc(
        &crate::metrics::REQUEST_BYTES,
        &[&user_id, &model_label],
        body_len as f64,
    );
    // Series this request updates only when it finishes start at zero now
    // (see `Metrics::touch`).
    state
        .metrics
        .touch(&crate::metrics::REQUESTS, &[&user_id, &model_label, endpoint, "ok"]);
    state
        .metrics
        .touch_histogram(&crate::metrics::QUEUE_WAIT, &[&user_id, &model_label]);

    state.reqlog.log(crate::reqlog::ReqRecord {
        ts: crate::reqlog::now_unix_millis(),
        dir: "IN",
        user: addr.to_string(),
        model: requested_model.clone(),
        backend: None,
        method: task_method.clone(),
        path: path.clone(),
        status: None,
        bytes: Some(body_len),
        content_type: headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        content: Some(content.clone()),
    });

    state.log_event(crate::dispatcher::LogEvent {
        at: std::time::SystemTime::now(),
        dir: "IN",
        user: user_id.clone(),
        model: requested_model.clone(),
        backend: None,
        info: format!("{} {} body={}B", task_method, path, body_len),
        content: Some(content),
    });

    state.notify.notify_one();

    let mut rx = rx;
    match rx.recv().await {
        Some(ResponsePart::Status(status, headers)) => {
            let stream = ReceiverStream::new(rx).map(|part| match part {
                ResponsePart::Chunk(chunk) => Ok(chunk),
                ResponsePart::Error(e) => Err(e),
                _ => Ok(Bytes::new()),
            });

            let mut res = Body::from_stream(stream).into_response();
            *res.status_mut() = status;
            *res.headers_mut() = headers;
            res
        }
        Some(ResponsePart::Error(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Backend error: {}", e),
        )
            .into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Worker failed to respond",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StickyMode;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn metadata_paths_are_classified() {
        for p in [
            "/",
            "/api/tags",
            "/api/ps",
            "/api/version",
            "/api/show",
            "/v1/models",
            "/v1/models/llama3",
        ] {
            assert!(is_metadata_path(p), "{p} should be a metadata read");
        }
        // Anything that can run inference, transfer a model or mutate state
        // is scheduled normally.
        for p in [
            "/api/generate",
            "/api/chat",
            "/api/embed",
            "/api/embeddings",
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/api/pull",
            "/api/push",
            "/api/create",
            "/api/copy",
            "/api/delete",
            "/api/blobs/sha256:abc",
        ] {
            assert!(!is_metadata_path(p), "{p} must not bypass scheduling");
        }
    }

    #[test]
    fn model_field_extracts_the_requested_model() {
        let parse = |b: &str| extract_requested_model(b.as_bytes());
        assert_eq!(
            parse(r#"{"model":"llama3","stream":true}"#).as_deref(),
            Some("llama3")
        );
        // Everything but `model` is skipped by the deserializer rather than
        // allocated — a big ignored field must not change the outcome.
        let big = format!(
            r#"{{"messages":[{{"role":"user","content":"{}"}}],"model":"qwen3"}}"#,
            "x".repeat(10_000)
        );
        assert_eq!(parse(&big).as_deref(), Some("qwen3"));
        // Field order must not matter either.
        assert_eq!(parse(r#"{"model":"a","options":{"num_ctx":8}}"#).as_deref(), Some("a"));

        // Leading whitespace is fine.
        assert_eq!(parse("  \n\t{\"model\":\"b\"}").as_deref(), Some("b"));

        // No model, not an object, not JSON, empty → None, as before.
        assert_eq!(parse(r#"{"stream":true}"#), None);
        // A top-level array must NOT be read positionally into `model`.
        assert_eq!(parse(r#"["llama3"]"#), None);
        assert_eq!(parse(r#""llama3""#), None);
        assert_eq!(parse("not json at all"), None);
        assert_eq!(parse(""), None);
        // A non-string `model` is not a usable name.
        assert_eq!(parse(r#"{"model":123}"#), None);
        assert_eq!(parse(r#"{"model":null}"#), None);
    }

    #[test]
    fn pair_matching_handles_tags_and_case() {
        // Case-insensitive, tag-stripped — but publisher-strict at pair level;
        // cross-publisher matching happens in model_routable via base compare.
        assert!(smart_model_match_one("qwen3.8-27b", "QWEN3.8-27B"));
        assert!(smart_model_match_one("llama3:latest", "llama3"));
        assert!(!smart_model_match_one("mistral", "llama3:latest"));
    }

    #[test]
    fn normalized_matching_strips_owner_and_quant() {
        // Client asks "qwen3.8-27b"; server lists publisher/quant variants of it.
        assert!(model_routable("qwen3.8-27b", &set(&["unsloth/qwen3.8-27b@q8_0"])));
        assert!(model_routable("qwen3.8-27b", &set(&["qwen/qwen3.8-27b"])));
        // Quant suffixes on either side are normalized away (same model family).
        assert!(model_routable(
            "unsloth/qwen3.8-27b@q8_0",
            &set(&["unsloth/qwen3.8-27b@q4_k_m"])
        ));
    }

    #[test]
    fn matching_rejects_substring_lookalikes() {
        // No arbitrary substring matching: these share name fragments but are
        // different models and must NOT be routable for "qwen3.8-27b".
        let lookalikes = set(&[
            "huihui-qwen3.8-27b-abliterated",
            "ovisocr2@q8_0",
            "llama3:latest",
            "mistral-7b",
            "text-embedding-nomic",
        ]);
        assert!(!model_routable("qwen3.8-27b", &lookalikes));
    }

    #[test]
    fn owner_prefixed_requests_are_strict() {
        // A request that names a publisher must reach a backend listing that
        // exact id (up to tag/quant normalization), not just the same base name.
        let avail = set(&["qwen/qwen3.8-27b", "unsloth/qwen3.8-27b@q8_0"]);
        assert!(model_routable("unsloth/qwen3.8-27b@q8_0", &avail));
        assert!(!model_routable(
            "unsloth/qwen3.8-27b@q4_k_m",
            &set(&["qwen/qwen3.8-27b"])
        ));
    }

    #[test]
    fn bare_names_match_across_publishers() {
        // "llama3" reaches meta-llama/llama3:latest (publisher stripped), but a
        // different base name does not match on prefix alone.
        assert!(model_routable("llama3", &set(&["meta-llama/llama3:latest"])));
        assert!(!model_routable("llama3", &set(&["meta-llama/llama3.1:latest"])));
    }

    fn backend_with(api_type: BackendApiType, rejected: &[ApiFamily]) -> BackendStatus {
        BackendStatus {
            url: "http://test".into(),
            token: None,
            active_requests: 0,
            processed_count: 0,
            is_online: true,
            api_type,
            available_models: Arc::default(),
            loaded_models: Arc::default(),
            loaded_state_known: false,
            loaded_ctx: HashMap::new(),
            current_model: None,
            active_by_model: HashMap::new(),
            lmstudio: false,
            native_models: Arc::default(),
            known_bad_endpoints: HashSet::new(),
            rejected_families: rejected.iter().copied().collect(),
            family_fail_counts: HashMap::new(),
            forced_api: BackendApiType::EMPTY,
            declared_models: Vec::new(),
            detected_slots: None,
            kv_free_pct: None,
            name: None,
            disabled: false,
        }
    }

    #[test]
    fn model_loaded_on_matches_ollama_ps() {
        let mut b = backend_with(BackendApiType::Ollama, &[]);
        b.loaded_state_known = true;
        b.loaded_models = Arc::new(set(&["qwen3.8-27b:latest"]));
        assert!(model_loaded_on(&b, "qwen3.8-27b"));
        assert!(!model_loaded_on(&b, "other-model"));
    }

    #[test]
    fn model_loaded_on_matches_lmstudio_key_with_quant() {
        let mut b = backend_with(BackendApiType::OpenAi, &[]);
        b.lmstudio = true;
        b.loaded_state_known = true;
        b.loaded_models = Arc::new(set(&["unsloth/qwen3.8-27b@q8_0"]));
        // Bare request name reaches the publisher/quant-prefixed loaded key.
        assert!(model_loaded_on(&b, "qwen3.8-27b"));
    }

    #[test]
    fn model_loaded_on_matches_lmstudio_display_name() {
        let mut b = backend_with(BackendApiType::OpenAi, &[]);
        b.lmstudio = true;
        b.loaded_state_known = true;
        b.native_models = Arc::new(vec![LmModelInfo {
            key: "unsloth/qwen3.8-27b@q8_0".into(),
            display_name: Some("Qwen3.8 27B".into()),
            loaded_instance_ids: vec!["unsloth/qwen3.8-27b@q8_0".into()],
        }]);
        // The display name matches directly (no `:tag`/`@quant` on either side).
        assert!(model_loaded_on(&b, "Qwen3.8 27B"));
        // The key matches via the shared bounded rules (bare-name base compare).
        assert!(model_loaded_on(&b, "qwen3.8-27b"));
    }

    #[test]
    fn model_loaded_on_ignores_unloaded_native_entries() {
        let mut b = backend_with(BackendApiType::OpenAi, &[]);
        b.lmstudio = true;
        b.loaded_state_known = true;
        b.native_models = Arc::new(vec![LmModelInfo {
            key: "unsloth/qwen3.8-27b@q8_0".into(),
            display_name: Some("Qwen3.8 27B".into()),
            loaded_instance_ids: Vec::new(),
        }]);
        // Listed but with no loaded instances: not resident.
        assert!(!model_loaded_on(&b, "qwen3.8-27b"));
        assert!(!model_loaded_on(&b, "Qwen3.8 27B"));
    }

    #[test]
    fn model_loaded_on_no_match() {
        let mut b = backend_with(BackendApiType::OpenAi, &[]);
        b.lmstudio = true;
        b.loaded_state_known = true;
        b.loaded_models = Arc::new(set(&["ovisocr2@q4_k_m"]));
        assert!(!model_loaded_on(&b, "qwen3.8-27b"));
    }

    #[test]
    fn endpoint_rejection_heuristic() {
        // 405 is always structural, regardless of the body.
        assert!(is_endpoint_rejection(StatusCode::METHOD_NOT_ALLOWED, ""));
        assert!(is_endpoint_rejection(
            StatusCode::METHOD_NOT_ALLOWED,
            "whatever"
        ));
        // LM Studio's signature: 200 + error JSON mentioning endpoint/method.
        let lm = r#"{"error":"Unexpected endpoint or method. (GET /api/tags)"}"#;
        assert!(is_endpoint_rejection(StatusCode::OK, lm));
        // Normal application errors are not dialect rejections.
        assert!(!is_endpoint_rejection(
            StatusCode::NOT_FOUND,
            r#"{"error":"model 'x' not found"}"#
        ));
        assert!(!is_endpoint_rejection(
            StatusCode::INTERNAL_SERVER_ERROR,
            "boom"
        ));
        assert!(!is_endpoint_rejection(StatusCode::OK, r#"{"message":"ok"}"#));

        // A body prefix truncated mid-JSON is NOT evidence either way: real
        // rejections are short and parse whole. This is the first 512 bytes of
        // a streaming answer that happens to discuss HTTP methods.
        let truncated = r#"{"model":"llama3","created_at":"2026-01-01T00:00:00Z","response":"The endpoint you want depends on the request method"#;
        assert!(!is_endpoint_rejection(StatusCode::OK, truncated));
        // Same shape, but complete and with no error field: still not one.
        let complete = r#"{"model":"llama3","response":"use the POST method on that endpoint"}"#;
        assert!(!is_endpoint_rejection(StatusCode::OK, complete));
        // A non-string error field is not a message to match on.
        assert!(!is_endpoint_rejection(StatusCode::OK, r#"{"error":{"code":42}}"#));
    }

    #[test]
    fn family_compatibility_rules() {
        // Unknown type is allowed until the health check classifies it.
        assert!(family_compatible(
            &backend_with(BackendApiType::Unknown, &[]),
            ApiSet::OLLAMA
        ));
        // Ollama-only backend cannot serve OpenAI-family requests (and vice versa).
        assert!(!family_compatible(
            &backend_with(BackendApiType::Ollama, &[]),
            ApiSet::OPENAI
        ));
        assert!(family_compatible(
            &backend_with(BackendApiType::Ollama, &[]),
            ApiSet::OLLAMA
        ));
        // Both = Ollama + OpenAI: everything the two-family world had.
        assert!(family_compatible(
            &backend_with(BackendApiType::Both, &[]),
            ApiSet::OPENAI
        ));
        assert!(family_compatible(
            &backend_with(BackendApiType::Both, &[]),
            ApiSet::OLLAMA
        ));
        // …but it does not include the new family: that needs its own bit.
        assert!(!family_compatible(
            &backend_with(BackendApiType::Both, &[]),
            ApiSet::SYSTEMONE
        ));
        assert!(family_compatible(
            &backend_with(ApiSet::ALL, &[]),
            ApiSet::SYSTEMONE
        ));
        // Learned rejections override the detected type.
        let b = backend_with(BackendApiType::Both, &[ApiFamily::Ollama]);
        assert!(!family_compatible(&b, ApiSet::OLLAMA));
        assert!(family_compatible(&b, ApiSet::OPENAI));
    }

    #[test]
    fn api_set_is_a_bitset() {
        let both = ApiSet::OLLAMA.union(ApiSet::OPENAI);
        assert_eq!(both, ApiSet::BOTH);
        assert!(both.contains(ApiFamily::Ollama) && both.contains(ApiFamily::OpenAi));
        assert!(!both.contains(ApiFamily::SystemOne));
        assert!(ApiSet::EMPTY.is_empty());
        assert!(!both.is_empty());
        assert_eq!(ApiSet::OLLAMA.intersection(ApiSet::OPENAI), ApiSet::EMPTY);
        assert_eq!(ApiSet::OLLAMA.intersection(ApiSet::BOTH), ApiSet::OLLAMA);
        // A probe only ever adds flavours, and a set no enum variant could have
        // named is representable: Ollama + OpenAI + System One.
        assert_eq!(ApiSet::Unknown.merge(ApiSet::Ollama), ApiSet::Ollama);
        assert_eq!(ApiSet::Ollama.merge(ApiSet::OpenAi), ApiSet::BOTH);
        let everything = ApiSet::BOTH.merge(ApiSet::SYSTEMONE);
        assert_eq!(everything, ApiSet::ALL);
        // Only a single-family set names one family; mixed and empty sets do not.
        assert_eq!(ApiSet::SYSTEMONE.single(), Some(ApiFamily::SystemOne));
        assert_eq!(ApiSet::BOTH.single(), None);
        assert_eq!(ApiSet::EMPTY.single(), None);
        assert_eq!(ApiSet::SYSTEMONE.display(), "SystemOne");
        assert_eq!(ApiSet::EMPTY.display(), "???");
        assert_eq!(format!("{:?}", ApiSet::BOTH), "O+OA");
        assert_eq!(format!("{:?}", everything), "O+OA+SO");
    }

    /// The `kind:` config key resolves leniently (a typo in a config file is
    /// invisible) but never guesses: anything that is not a known family name —
    /// including an empty value — pins nothing, leaving the backend to probe
    /// classification.
    #[test]
    fn api_set_from_kind() {
        assert_eq!(
            ApiSet::from_kind(Some("systemone")),
            Some(ApiSet::SYSTEMONE)
        );
        assert_eq!(
            ApiSet::from_kind(Some("SystemOne")),
            Some(ApiSet::SYSTEMONE)
        );
        assert_eq!(
            ApiSet::from_kind(Some(" system-one ")),
            Some(ApiSet::SYSTEMONE)
        );
        assert_eq!(
            ApiSet::from_kind(Some("SYSTEM_ONE")),
            Some(ApiSet::SYSTEMONE)
        );
        assert_eq!(ApiSet::from_kind(Some("ollama")), None);
        assert_eq!(ApiSet::from_kind(Some("")), None);
        assert_eq!(ApiSet::from_kind(Some("   ")), None);
        assert_eq!(ApiSet::from_kind(None), None);
    }

    /// The prefix rule that decided routing before the System One family
    /// existed, reproduced verbatim from the old `detect_api_family` (including
    /// its `ApiFamily::Unknown`, now `ApiSet::ALL`), so the route table below
    /// is checked against the *behaviour being preserved* rather than against
    /// the new implementation.
    fn legacy_route_families(path: &str) -> ApiSet {
        if path.starts_with("/api/") {
            ApiSet::OLLAMA
        } else if path.starts_with("/v1/") {
            ApiSet::OPENAI
        } else {
            ApiSet::ALL
        }
    }

    /// THE REGRESSION GUARD. Every proxy route registered in `main.rs`
    /// (each is `any(proxy_handler)`; the method column records how clients
    /// actually use it) with the family set it must be routed with. A row that
    /// changes family means existing Ollama / LM Studio / OpenAI traffic
    /// changes which backends it can reach.
    /// The route's own answer envelope is the FIRST declared listing shape (see
    /// `aggregate_model_list`); the remaining shapes are accepted on input only.
    /// `/v1/models` therefore always answers OpenAI's `data`/`id` and `/api/tags`
    /// always Ollama's `models`/`name`, whatever the backends answer with.
    #[test]
    fn each_listing_route_answers_in_its_own_envelope() {
        assert_eq!(
            model_list_shapes("/v1/models").expect("v1 models is a listing")[0],
            ("data", "id")
        );
        assert_eq!(
            model_list_shapes("/api/tags").expect("api tags is a listing")[0],
            ("models", "name")
        );
        assert!(model_list_shapes("/v1/chat/completions").is_none());
    }

    #[test]
    fn route_table_keeps_every_registered_routes_family() {
        let models_pair = ApiSet::OPENAI.union(ApiSet::SYSTEMONE);
        let table: Vec<(Method, &str, ApiSet)> = vec![
            // Root: "Ollama is running" — any backend may answer, as before.
            (Method::GET, "/", ApiSet::ALL),
            // Ollama API endpoints.
            (Method::POST, "/api/generate", ApiSet::OLLAMA),
            (Method::POST, "/api/chat", ApiSet::OLLAMA),
            (Method::POST, "/api/embed", ApiSet::OLLAMA),
            (Method::POST, "/api/embeddings", ApiSet::OLLAMA),
            (Method::GET, "/api/tags", ApiSet::OLLAMA),
            (Method::POST, "/api/show", ApiSet::OLLAMA),
            (Method::POST, "/api/create", ApiSet::OLLAMA),
            (Method::POST, "/api/copy", ApiSet::OLLAMA),
            (Method::POST, "/api/delete", ApiSet::OLLAMA),
            (Method::POST, "/api/pull", ApiSet::OLLAMA),
            (Method::POST, "/api/push", ApiSet::OLLAMA),
            (Method::PUT, "/api/blobs/sha256:aaaa", ApiSet::OLLAMA),
            (Method::GET, "/api/ps", ApiSet::OLLAMA),
            (Method::GET, "/api/version", ApiSet::OLLAMA),
            // OpenAI-compatible endpoints.
            (Method::POST, "/v1/chat/completions", ApiSet::OPENAI),
            (Method::POST, "/v1/completions", ApiSet::OPENAI),
            (Method::POST, "/v1/embeddings", ApiSet::OPENAI),
            // Listing endpoints. `/v1/models` (and `/v1/models/{model}`) is now
            // shared with the System One servers, which answer it with
            // `{"models":[{"name":…}]}` instead of OpenAI's envelope.
            (Method::GET, "/v1/models", models_pair),
            (Method::GET, "/v1/models/clm-latest", models_pair),
            // NEW in P1: the decision-model route. The old prefix rule handed
            // it to OpenAI backends, which 404 it — it must reach System One
            // backends and nothing else.
            (Method::POST, "/v1/systemone", ApiSet::SYSTEMONE),
        ];

        for (method, path, expected) in &table {
            let got = families_for_route(method, path);
            assert_eq!(&got, expected, "family set of {} changed", path);
            // `any(proxy_handler)` means the family may not depend on method.
            assert_eq!(
                families_for_route(&Method::GET, path),
                got,
                "{}: family must not depend on the method",
                path
            );
        }

        // Every row except the deliberately changed ones must be exactly what
        // the old prefix rule decided — and a row listed as changed must
        // actually differ, so this list cannot silently go stale.
        let changed = ["/v1/models", "/v1/models/clm-latest", "/v1/systemone"];
        for (_, path, expected) in &table {
            if changed.contains(path) {
                assert_ne!(
                    *expected,
                    legacy_route_families(path),
                    "{}: listed as changed but agrees with the legacy rule — stale row",
                    path
                );
            } else {
                assert_eq!(
                    *expected,
                    legacy_route_families(path),
                    "{}: must keep the family it has today",
                    path
                );
            }
        }
    }

    #[test]
    fn unregistered_paths_keep_todays_classification() {
        // Paths that are not in main.rs's route table: reachable only through
        // the optional `allow_all_routes` fallback (`/v1/rank`, the Kev/CLM
        // extra routes — P3 work), never proxied (`/health`, `/admin/*`: both
        // are served by ollamaMQ itself), or simply unknown. P1 changes none.
        for path in [
            "/v1/rank",
            "/v1/systemone/permute",
            "/v1/systemone/separate",
            "/health",
            "/admin/models",
            "/made/up",
            "/v1",
        ] {
            assert_eq!(
                families_for_route(&Method::POST, path),
                legacy_route_families(path),
                "{}: classification must not change in P1",
                path
            );
        }
    }

    #[test]
    fn system_one_and_ollama_backends_are_not_interchangeable() {
        // The point of the new family: a decision-model backend implements no
        // `/api/*` route at all, so it must never be handed Ollama traffic...
        let sysone = backend_with(ApiSet::SYSTEMONE, &[]);
        assert!(!family_compatible(
            &sysone,
            families_for_route(&Method::POST, "/api/chat")
        ));
        assert!(!family_compatible(
            &sysone,
            families_for_route(&Method::GET, "/api/tags")
        ));
        // ...while serving its own route and the shared listing endpoint.
        assert!(family_compatible(
            &sysone,
            families_for_route(&Method::POST, "/v1/systemone")
        ));
        assert!(family_compatible(
            &sysone,
            families_for_route(&Method::GET, "/v1/models")
        ));

        // An Ollama backend must not be handed the decision-model route.
        let ollama = backend_with(BackendApiType::Ollama, &[]);
        assert!(!family_compatible(
            &ollama,
            families_for_route(&Method::POST, "/v1/systemone")
        ));
        assert!(family_compatible(
            &ollama,
            families_for_route(&Method::POST, "/api/chat")
        ));
        // An Ollama-only backend was never eligible for /v1/models and is still
        // not: only OpenAI and System One servers answer it.
        assert!(!family_compatible(
            &ollama,
            families_for_route(&Method::GET, "/v1/models")
        ));

        // Nor an OpenAI backend — that is the misroute this family exists to end.
        let openai = backend_with(BackendApiType::OpenAi, &[]);
        assert!(!family_compatible(
            &openai,
            families_for_route(&Method::POST, "/v1/systemone")
        ));
        assert!(family_compatible(
            &openai,
            families_for_route(&Method::POST, "/v1/chat/completions")
        ));

        // A learned rejection of one family must not cost a backend its duty in
        // another: `/v1/models` is answered by OpenAI and System One servers.
        let openai_rejecting_openai = backend_with(BackendApiType::OpenAi, &[ApiFamily::OpenAi]);
        assert!(!family_compatible(
            &openai_rejecting_openai,
            families_for_route(&Method::GET, "/v1/models")
        ));
        let dual_rejecting_sysone = backend_with(
            ApiSet::OPENAI.union(ApiSet::SYSTEMONE),
            &[ApiFamily::SystemOne],
        );
        assert!(family_compatible(
            &dual_rejecting_sysone,
            families_for_route(&Method::GET, "/v1/models")
        ));
    }

    #[test]
    fn family_learning_blames_the_family_the_route_named() {
        let openai = backend_with(BackendApiType::OpenAi, &[]);
        // Single-family routes blame the route's family, as before — even when
        // the backend would not have been served by it.
        assert_eq!(
            learnable_family(&openai, families_for_route(&Method::POST, "/api/chat")),
            Some(ApiFamily::Ollama)
        );
        assert_eq!(
            learnable_family(
                &openai,
                families_for_route(&Method::POST, "/v1/chat/completions")
            ),
            Some(ApiFamily::OpenAi)
        );
        // The shared listing endpoint is pinned down by the backend's own type.
        assert_eq!(
            learnable_family(&openai, families_for_route(&Method::GET, "/v1/models")),
            Some(ApiFamily::OpenAi)
        );
        assert_eq!(
            learnable_family(
                &backend_with(ApiSet::SYSTEMONE, &[]),
                families_for_route(&Method::GET, "/v1/models")
            ),
            Some(ApiFamily::SystemOne)
        );
        // A catch-all route and an unclassified backend teach nothing, as before.
        assert_eq!(
            learnable_family(&openai, families_for_route(&Method::GET, "/")),
            None
        );
        assert_eq!(
            learnable_family(
                &backend_with(BackendApiType::Unknown, &[]),
                families_for_route(&Method::GET, "/v1/models")
            ),
            None
        );
    }

    #[test]
    fn model_limits_are_normalized_and_lookup_defaults() {
        let configs = vec![crate::config::ModelConfig {
            name: "gpt-oss:120b".into(),
            identifier: None,
            max_ctx: None,
            keep_alive: None,
            max_concurrent_requests: 3,
            backends: Vec::new(),
        }];
        let limits = build_model_limits(&configs);
        // ':tag' is stripped when keying.
        assert_eq!(limits.get("gpt-oss"), Some(&3));
        // Lookup happens on the normalized (lowercased) key; unlisted models
        // fall back to 1 at lookup time in the scheduler.
        assert_eq!(limits.get("GPT-OSS"), None);
        assert_eq!(limits.get("qwen2.5-coder"), None);
    }

    #[test]
    fn model_limit_key_survives_publisher_and_quant_spellings() {
        // appconf.yaml.example pins this model with a publisher prefix while
        // clients ask for the bare name; both must land on the same key, or
        // the configured limit silently degrades to 1 and the two spellings
        // get separate in-flight budgets on one backend.
        let configs = vec![crate::config::ModelConfig {
            name: "qwen/qwen3.8-27b".into(),
            identifier: None,
            max_ctx: None,
            keep_alive: None,
            max_concurrent_requests: 3,
            backends: Vec::new(),
        }];
        let limits = build_model_limits(&configs);
        for requested in [
            "qwen3.8-27b",
            "qwen/qwen3.8-27b",
            "unsloth/qwen3.8-27b@q8_0",
            "QWEN3.8-27B:latest",
        ] {
            assert_eq!(
                limits.get(&model_concurrency_key(requested)),
                Some(&3),
                "'{requested}' must resolve to the configured limit"
            );
        }
        // A different model still gets its own (default) budget.
        assert_ne!(
            model_concurrency_key("qwen3.8-27b"),
            model_concurrency_key("qwen3.8-32b")
        );
        assert_eq!(limits.get(&model_concurrency_key("llama3")), None);
    }

    #[test]
    fn backend_capacity_enforces_global_and_per_model() {
        let mut b = backend_with(BackendApiType::Ollama, &[]);
        let cap: u32 = 3;
        assert!(backend_has_capacity(&b, Some("a"), 2, cap));

        // Two in-flight for "a" (at its limit) and one for "b": global cap hit.
        b.active_requests = 3;
        b.active_by_model.insert("a".into(), 2);
        b.active_by_model.insert("b".into(), 1);
        assert!(!backend_has_capacity(&b, Some("c"), 5, cap)); // cap reached

        // One slot frees up: "a" is at its per-model limit, others still fit.
        b.active_requests = 2;
        assert!(!backend_has_capacity(&b, Some("a"), 2, cap));
        assert!(backend_has_capacity(&b, Some("b"), 3, cap));
        // Model-less requests are bounded by the global cap only.
        assert!(backend_has_capacity(&b, None, 0, cap));
    }

    #[test]
    fn backend_capacity_defaults_to_single_slot() {
        let mut b = backend_with(BackendApiType::Ollama, &[]);
        // Historical behavior: one in-flight request per backend.
        assert!(backend_has_capacity(&b, Some("m"), 1, 1));
        b.active_requests = 1;
        assert!(!backend_has_capacity(&b, Some("m"), 5, 1)); // global cap binds first
    }

    #[tokio::test]
    async fn stuck_request_fails_fast_with_503() {
        // Backend that can never serve the requested model (nothing listens).
        let state = Arc::new(AppState::new(
            vec![("http://127.0.0.1:9".to_string(), None)],
            5,     // request timeout
            86400, // load keep alive
            1,     // stuck_timeout: fail fast after 1s
            1,     // max concurrent per backend
            "appconf.yaml".into(),
            Vec::new(),
            crate::reqlog::RequestLogger::disabled(),
            65_536,
            512 * 1024 * 1024,
        ));

        let (tx, mut rx) = mpsc::channel(32);
        {
            let mut queues = state.queues.lock_or_recover();
            queues.entry("tester".to_string()).or_default().push_back(Task {
                method: Method::POST,
                user: "127.0.0.1:41000".to_string(),
                path: "/api/chat".into(),
                headers: HeaderMap::new(),
                body: Bytes::from(r#"{"model":"no-such-model-xyz","messages":[]}"#),
                responder: tx,
                requested_model: Some("no-such-model-xyz".to_string()),
                stuck_warned: false,
                cache_passes: 0,
                queued_at: std::time::Instant::now(),
                key_label: None,
                session_key: None,
            });
        }

        // Enqueued before the worker starts: its first scan pass sees it.
        let worker = tokio::spawn(run_worker(state.clone()));

        let part = tokio::time::timeout(std::time::Duration::from_secs(6), rx.recv())
            .await
            .expect("no response within 6s")
            .expect("responder closed");
        match part {
            ResponsePart::Status(status, _) => {
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE)
            }
            _ => panic!("expected Status(503), got another part"),
        }

        worker.abort();
    }

    /// Backend mock that records `(path, Authorization header)` for every
    /// request and answers 200. `/api/tags` returns an Ollama-style listing
    /// so the health probes keep `llama3` available on it.
    #[allow(clippy::type_complexity)]
    async fn spawn_recording_backend()
    -> (String, Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>) {
        let calls: Arc<std::sync::Mutex<Vec<(String, Option<String>)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind recording backend");
        let url = format!("http://{}", listener.local_addr().expect("mock local addr"));
        let c = calls.clone();
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/{*path}",
                axum::routing::any(move |uri: axum::extract::OriginalUri, headers: HeaderMap| {
                    let c = c.clone();
                    async move {
                        let path = uri.0.path().to_string();
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        c.lock_or_recover().push((path.clone(), auth));
                        if path == "/api/tags" {
                            axum::Json(serde_json::json!({ "models": [{ "name": "llama3" }] }))
                        } else {
                            axum::Json(serde_json::json!({ "ok": true }))
                        }
                    }
                }),
            );
            let _ = axum::serve(listener, app).await;
        });
        (url, calls)
    }

    /// Enqueue one `POST /api/chat` request for `llama3` under user "tester"
    /// with the given client headers. Returns the receiver — keep it alive
    /// so the worker doesn't drop the request as client-gone.
    fn enqueue_llama_chat(
        state: &Arc<AppState>,
        headers: HeaderMap,
    ) -> mpsc::Receiver<ResponsePart> {
        let (tx, rx) = mpsc::channel(32);
        let body = Bytes::from(r#"{"model":"llama3","messages":[]}"#);
        state
            .queued_bytes
            .fetch_add(body.len() as u64, std::sync::atomic::Ordering::Relaxed);
        state
            .queues
            .lock_or_recover()
            .entry("tester".to_string())
            .or_default()
            .push_back(Task {
                method: Method::POST,
                user: "127.0.0.1:41000".to_string(),
                path: "/api/chat".into(),
                headers,
                body,
                responder: tx,
                requested_model: Some("llama3".to_string()),
                stuck_warned: false,
                cache_passes: 0,
                queued_at: std::time::Instant::now(),
                key_label: None,
                session_key: None,
            });
        rx
    }

    /// Mark the single test backend as an online Ollama that lists `llama3`
    /// (the state its mock's probe answers produce), with the given token.
    fn arm_ollama_backend(state: &Arc<AppState>, token: Option<String>) {
        let mut backends = state.backends.lock_or_recover();
        backends[0].token = token;
        backends[0].is_online = true;
        backends[0].api_type = BackendApiType::Ollama;
        backends[0].available_models = Arc::new(set(&["llama3"]));
    }

    /// The configured backend token is attached to proxied requests that carry
    /// no client credentials of their own.
    #[tokio::test]
    async fn proxy_attaches_configured_backend_token() {
        let (url, calls) = spawn_recording_backend().await;
        let state = new_test_state(vec![url.clone()]);
        arm_ollama_backend(&state, Some("sekrit".to_string()));

        let _rx = enqueue_llama_chat(&state, HeaderMap::new());
        let worker = tokio::spawn(run_worker(state.clone()));

        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 0);
        wait_for_completion(&state, picked).await;
        worker.abort();

        let calls = calls.lock_or_recover().clone();
        let chat: Vec<_> = calls.into_iter().filter(|(p, _)| p == "/api/chat").collect();
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].1.as_deref(), Some("Bearer sekrit"));
    }

    /// A client's own Authorization header is forwarded untouched — the
    /// configured backend token must not override it.
    #[tokio::test]
    async fn proxy_does_not_override_client_auth_header() {
        let (url, calls) = spawn_recording_backend().await;
        let state = new_test_state(vec![url.clone()]);
        arm_ollama_backend(&state, Some("sekrit".to_string()));

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer client-key".parse().expect("valid header value"),
        );
        let _rx = enqueue_llama_chat(&state, headers);
        let worker = tokio::spawn(run_worker(state.clone()));

        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 0);
        wait_for_completion(&state, picked).await;
        worker.abort();

        let calls = calls.lock_or_recover().clone();
        let chat: Vec<_> = calls.into_iter().filter(|(p, _)| p == "/api/chat").collect();
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].1.as_deref(), Some("Bearer client-key"));
    }

    fn add_test_client(state: &Arc<AppState>, name: &str, key: &str) {
        let mut clients = state.clients.lock_or_recover();
        clients.create_client(name, "").expect("create client");
        clients.add_key(name, Some(key), "").expect("add key");
    }

    /// A client key picks the queue (over the client's own `X-User-ID`) and
    /// is stripped before forwarding, so the backend token applies instead.
    #[tokio::test]
    async fn client_key_sets_queue_user_and_is_not_forwarded() {
        let (url, calls) = spawn_recording_backend().await;
        let state = new_test_state(vec![url]);
        arm_ollama_backend(&state, Some("sekrit".to_string()));
        add_test_client(&state, "alice", "alice-key-123");
        let worker = tokio::spawn(run_worker(state.clone()));

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer alice-key-123".parse().expect("header value"),
        );
        headers.insert("X-User-ID", "mallory".parse().expect("header value"));
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            proxy_handler(
                State(state.clone()),
                ConnectInfo("127.0.0.1:41010".parse().expect("addr")),
                Method::POST,
                headers,
                axum::extract::OriginalUri("/api/chat".parse().expect("uri")),
                Bytes::from_static(br#"{"model":"llama3","messages":[]}"#),
            ),
        )
        .await
        .expect("request not answered within 10s")
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        let _ = axum::body::to_bytes(res.into_body(), usize::MAX).await;
        worker.abort();

        let ips = state.user_ips.lock_or_recover();
        assert!(ips.contains_key("alice"), "queued under {:?}", ips.keys());
        assert!(!ips.contains_key("mallory"));
        drop(ips);
        let calls = calls.lock_or_recover().clone();
        let chat: Vec<_> = calls.into_iter().filter(|(p, _)| p == "/api/chat").collect();
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].1.as_deref(), Some("Bearer sekrit"));
    }

    #[tokio::test]
    async fn disabled_client_key_is_rejected() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        add_test_client(&state, "bob", "bob-key-0001");
        state
            .clients
            .lock_or_recover()
            .update_client(
                "bob",
                crate::clients::ClientPatch {
                    disabled: Some(true),
                    ..Default::default()
                },
            )
            .expect("disable");

        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "bob-key-0001".parse().expect("header value"));
        let res = proxy_handler(
            State(state.clone()),
            ConnectInfo("127.0.0.1:41011".parse().expect("addr")),
            Method::POST,
            headers,
            axum::extract::OriginalUri("/api/chat".parse().expect("uri")),
            Bytes::from_static(br#"{"model":"llama3"}"#),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(state.queues.lock_or_recover().is_empty());
    }

    /// A key no client owns changes nothing: `X-User-ID` still decides.
    /// (Observed through a block on that user, so no backend is needed.)
    #[tokio::test]
    async fn unknown_key_falls_back_to_user_header() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        add_test_client(&state, "alice", "alice-key-123");
        // Set directly: `block_user` would persist to the working directory.
        state.blocked_users.lock_or_recover().insert("carol".to_string());

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer not-a-client-key".parse().expect("header value"),
        );
        headers.insert("X-User-ID", "carol".parse().expect("header value"));
        let res = proxy_handler(
            State(state.clone()),
            ConnectInfo("127.0.0.1:41012".parse().expect("addr")),
            Method::POST,
            headers,
            axum::extract::OriginalUri("/api/chat".parse().expect("uri")),
            Bytes::from_static(br#"{"model":"llama3"}"#),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(&body[..], b"User blocked");
    }

    /// A keyless request may not take a registered client's name through
    /// `X-User-ID`: it is queued apart as `unverified:<name>`.
    #[tokio::test]
    async fn unkeyed_claim_of_client_name_is_queued_apart() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        add_test_client(&state, "alice", "alice-key-123");

        let mut headers = HeaderMap::new();
        headers.insert("X-User-ID", "alice".parse().expect("header value"));
        // No backend can serve it: it waits out stuck_timeout (1 s), then 503.
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            proxy_handler(
                State(state.clone()),
                ConnectInfo("127.0.0.1:41013".parse().expect("addr")),
                Method::POST,
                headers,
                axum::extract::OriginalUri("/api/chat".parse().expect("uri")),
                Bytes::from_static(br#"{"model":"llama3"}"#),
            ),
        );
        let worker = tokio::spawn(run_worker(state.clone()));
        let _ = res.await;
        worker.abort();

        let ips = state.user_ips.lock_or_recover();
        assert!(ips.contains_key("unverified:alice"), "got {:?}", ips.keys());
        assert!(!ips.contains_key("alice"));
    }

    #[test]
    fn blocking_a_client_disables_it() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        add_test_client(&state, "alice", "alice-key-123");

        state.block_user("alice".into());
        assert!(state.clients.lock_or_recover().is_disabled("alice"));
        assert!(state.blocked_users.lock_or_recover().is_empty());
        assert!(state.is_user_blocked("alice"));

        state.unblock_user("alice");
        assert!(!state.clients.lock_or_recover().is_disabled("alice"));
        assert!(!state.is_user_blocked("alice"));

        // Unregistered ids still use the user block list.
        state.block_user("bob".into());
        assert!(state.blocked_users.lock_or_recover().contains("bob"));
    }

    #[test]
    fn client_store_restores_priority_and_converts_blocks() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        add_test_client(&state, "alice", "alice-key-123");
        add_test_client(&state, "bob", "bob-key-0001");
        state
            .clients
            .lock_or_recover()
            .set_priority(Some("alice"), Some("bob"))
            .expect("priority");
        state.blocked_users.lock_or_recover().insert("bob".into());
        state.blocked_users.lock_or_recover().insert("carol".into());

        state.apply_client_store();
        assert_eq!(state.vip_user.lock_or_recover().as_deref(), Some("alice"));
        assert_eq!(state.boost_user.lock_or_recover().as_deref(), Some("bob"));
        assert!(state.clients.lock_or_recover().is_disabled("bob"));
        let blocked = state.blocked_users.lock_or_recover().clone();
        assert_eq!(blocked, ["carol".to_string()].into_iter().collect());
    }

    #[test]
    fn vip_and_boost_persist_on_clients_only() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        add_test_client(&state, "alice", "alice-key-123");
        state.toggle_vip("alice");
        assert_eq!(
            state.clients.lock_or_recover().priority(),
            (Some("alice".into()), None)
        );
        // An unregistered id gets the runtime toggle, which clears the saved one.
        state.toggle_vip("ip-10.0.0.9");
        assert_eq!(state.vip_user.lock_or_recover().as_deref(), Some("ip-10.0.0.9"));
        assert_eq!(state.clients.lock_or_recover().priority(), (None, None));
    }

    #[test]
    fn rename_moves_live_state() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        add_test_client(&state, "old", "rename-key-01");
        let (tx, _rx) = mpsc::channel(1);
        state.queues.lock_or_recover().entry("old".into()).or_default().push_back(Task {
            method: Method::POST,
            user: "127.0.0.1:41000".to_string(),
            path: "/api/chat".into(),
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"{}"),
            responder: tx,
            requested_model: None,
            stuck_warned: false,
            cache_passes: 0,
            queued_at: std::time::Instant::now(),
            key_label: None,
            session_key: None,
        });
        state.processed_counts.lock_or_recover().insert("old".into(), 7);
        state.toggle_boost("old");

        state
            .clients
            .lock_or_recover()
            .update_client(
                "old",
                crate::clients::ClientPatch {
                    name: Some("new".into()),
                    ..Default::default()
                },
            )
            .expect("rename");
        state.rename_identity("old", "new");

        assert_eq!(state.queues.lock_or_recover()["new"].len(), 1);
        assert!(!state.queues.lock_or_recover().contains_key("old"));
        assert_eq!(state.processed_counts.lock_or_recover()["new"], 7);
        assert_eq!(state.boost_user.lock_or_recover().as_deref(), Some("new"));
        assert_eq!(state.clients.lock_or_recover().priority(), (None, Some("new".into())));
    }

    fn hide_models(state: &Arc<AppState>, pattern: &str, backends: Vec<String>) {
        state
            .model_policy
            .lock_or_recover()
            .add_rule(pattern, crate::model_policy::Action::Hide, backends)
            .expect("add rule");
    }

    /// A hidden model is refused with 404 before queueing, in the error shape
    /// of the route's family, and a short name cannot reach a hidden listing.
    #[tokio::test]
    async fn hidden_model_is_refused_at_once() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        state.backends.lock_or_recover()[0].available_models = Arc::new(set(&["llama3:latest", "phi4:14b"]));
        hide_models(&state, "llama3:latest", vec![]);

        let call = |path: &'static str, body: &'static [u8]| {
            let state = state.clone();
            async move {
                proxy_handler(
                    State(state),
                    ConnectInfo("127.0.0.1:41020".parse().expect("addr")),
                    Method::POST,
                    HeaderMap::new(),
                    axum::extract::OriginalUri(path.parse().expect("uri")),
                    Bytes::from_static(body),
                )
                .await
                .into_response()
            }
        };
        let json = |res: axum::response::Response| async move {
            let b = axum::body::to_bytes(res.into_body(), usize::MAX).await.expect("body");
            serde_json::from_slice::<serde_json::Value>(&b).expect("json")
        };

        let res = call("/v1/chat/completions", br#"{"model":"llama3"}"#).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(json(res).await["error"]["code"], "model_not_found");

        let res = call("/api/chat", br#"{"model":"llama3:latest"}"#).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(json(res).await["error"], "model 'llama3:latest' not found");

        assert!(state.queues.lock_or_recover().is_empty(), "nothing was queued");
        let text = crate::metrics::render(&state);
        assert!(
            // No key, no X-User-ID: identified by address (`ip_identity`).
            text.contains(r#"ollamamq_requests_total{client="ip-127.0.0.1",model="llama3",endpoint="/v1/chat/completions",outcome="hidden_model"} 1"#),
            "{text}"
        );
        assert!(!state.model_blocked("phi4:14b"));
        // Not listed anywhere and not named by a rule: the normal path decides.
        assert!(!state.model_blocked("mistral"));
    }

    /// A rule scoped to one backend leaves the model routable on the other.
    #[tokio::test]
    async fn per_backend_hide_routes_to_the_other_backend() {
        let (url0, calls0) = spawn_recording_backend().await;
        let (url1, calls1) = spawn_recording_backend().await;
        let state = new_test_state(vec![url0.clone(), url1.clone()]);
        {
            let mut backends = state.backends.lock_or_recover();
            for b in backends.iter_mut() {
                b.is_online = true;
                b.api_type = BackendApiType::Ollama;
                b.available_models = Arc::new(set(&["llama3"]));
            }
        }
        hide_models(&state, "llama3", vec![url0.clone()]);
        assert!(!state.model_blocked("llama3"));

        let _rx = enqueue_llama_chat(&state, HeaderMap::new());
        let worker = tokio::spawn(run_worker(state.clone()));
        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 1);
        wait_for_completion(&state, picked).await;
        worker.abort();

        let chats0 = calls0.lock_or_recover().iter().filter(|(p, _)| p == "/api/chat").count();
        let chats1 = calls1.lock_or_recover().iter().filter(|(p, _)| p == "/api/chat").count();
        assert_eq!((chats0, chats1), (0, 1));
    }

    /// Hidden models are left out of the merged listing.
    #[tokio::test]
    async fn hidden_models_leave_the_merged_listing() {
        let (url, _calls) = spawn_recording_backend().await;
        let state = new_test_state(vec![url]);
        arm_ollama_backend(&state, None);
        let names = |res: axum::response::Response| async move {
            let b = axum::body::to_bytes(res.into_body(), usize::MAX).await.expect("body");
            let v: serde_json::Value = serde_json::from_slice(&b).expect("json");
            v["models"]
                .as_array()
                .expect("models array")
                .iter()
                .map(|m| m["name"].as_str().unwrap_or("").to_string())
                .collect::<Vec<_>>()
        };
        let res = aggregate_model_list(&state, "/api/tags").await.expect("listing");
        assert_eq!(names(res).await, vec!["llama3".to_string()]);

        hide_models(&state, "LLAMA*", vec![]);
        let res = aggregate_model_list(&state, "/api/tags").await.expect("listing");
        assert!(names(res).await.is_empty());
    }

    /// One request end to end: per-client/model counters, latency
    /// histograms and token usage parsed from the backend's answer.
    #[tokio::test]
    async fn metrics_record_a_served_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let app = axum::Router::new()
                .route(
                    "/api/tags",
                    axum::routing::get(|| async {
                        axum::Json(serde_json::json!({ "models": [{ "name": "llama3" }] }))
                    }),
                )
                .route(
                    "/api/chat",
                    axum::routing::post(|| async {
                        axum::Json(serde_json::json!({
                            "message": { "content": "hi" },
                            "done": true,
                            "prompt_eval_count": 7,
                            "eval_count": 9,
                            "prompt_eval_duration": 250_000_000u64,
                            "eval_duration": 300_000_000u64,
                        }))
                    }),
                );
            let _ = axum::serve(listener, app).await;
        });
        let state = new_test_state(vec![url.clone()]);
        arm_ollama_backend(&state, None);
        {
            let mut clients = state.clients.lock_or_recover();
            clients.create_client("alice", "").expect("client");
            clients
                .add_key("alice", Some("alice-key-123"), "laptop")
                .expect("key");
        }
        let worker = tokio::spawn(run_worker(state.clone()));

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer alice-key-123".parse().expect("header value"),
        );
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            proxy_handler(
                State(state.clone()),
                ConnectInfo("127.0.0.1:41030".parse().expect("addr")),
                Method::POST,
                headers,
                axum::extract::OriginalUri("/api/chat".parse().expect("uri")),
                Bytes::from_static(br#"{"model":"llama3","messages":[]}"#),
            ),
        )
        .await
        .expect("answered")
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        let _ = axum::body::to_bytes(res.into_body(), usize::MAX).await;
        wait_for_completion(&state, 0).await;
        worker.abort();

        let backend = crate::metrics::backend_label(&url);
        let text = crate::metrics::render(&state);
        for line in [
            r#"ollamamq_requests_total{client="alice",model="llama3",endpoint="/api/chat",outcome="ok"} 1"#.to_string(),
            format!(r#"ollamamq_backend_responses_total{{client="alice",model="llama3",backend="{backend}",code="200"}} 1"#),
            format!(r#"ollamamq_tokens_total{{client="alice",model="llama3",backend="{backend}",kind="prompt"}} 7"#),
            format!(r#"ollamamq_tokens_total{{client="alice",model="llama3",backend="{backend}",kind="completion"}} 9"#),
            r#"ollamamq_request_bytes_total{client="alice",model="llama3"} 32"#.to_string(),
            r#"ollamamq_queue_wait_seconds_count{client="alice",model="llama3"} 1"#.to_string(),
            format!(r#"ollamamq_request_duration_seconds_count{{model="llama3",backend="{backend}"}} 1"#),
            format!(r#"ollamamq_backend_up{{backend="{backend}",api_type="Ollama"}} 1"#),
            r#"ollamamq_client_key_tokens_total{client="alice",key="laptop",kind="completion"} 9"#.to_string(),
            r#"ollamamq_client_key_tokens_total{client="alice",key="laptop",kind="prompt"} 7"#.to_string(),
            format!(r#"ollamamq_backend_free_slots{{backend="{backend}"}} 1"#),
            // Speed from Ollama's durations: 9 tokens in 0.3 s = 30 tok/s.
            format!(r#"ollamamq_generation_seconds_total{{client="alice",model="llama3",backend="{backend}"}} 0.3"#),
            format!(r#"ollamamq_generation_tokens_total{{client="alice",model="llama3",backend="{backend}"}} 9"#),
            format!(r#"ollamamq_prompt_seconds_total{{client="alice",model="llama3",backend="{backend}"}} 0.25"#),
            format!(r#"ollamamq_prompt_eval_tokens_total{{client="alice",model="llama3",backend="{backend}"}} 7"#),
            format!(r#"ollamamq_generation_speed_tokens_per_second_bucket{{model="llama3",backend="{backend}",le="30"}} 1"#),
            format!(r#"ollamamq_generation_speed_tokens_per_second_bucket{{model="llama3",backend="{backend}",le="20"}} 0"#),
            // Not streamed: TTFT is the backend's prompt time.
            format!(r#"ollamamq_time_to_first_token_seconds_sum{{client="alice",model="llama3",backend="{backend}"}} 0.25"#),
        ] {
            assert!(text.contains(&line), "missing: {line}\n{text}");
        }
    }

    /// Waiting requests are reported by client and model, with the age of
    /// the oldest; an offline backend has no free slots.
    #[test]
    fn queue_and_slot_gauges() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        state.backends.lock_or_recover()[0].available_models = Arc::new(set(&["llama3"]));
        let _rx = enqueue_llama_chat(&state, HeaderMap::new());
        state.backends.lock_or_recover()[0].is_online = false;
        let text = crate::metrics::render(&state);
        assert!(
            text.contains(r#"ollamamq_queue_depth{client="tester",model="llama3"} 1"#),
            "{text}"
        );
        assert!(text.contains(r#"ollamamq_queue_oldest_seconds{client="tester",model="llama3"} "#));
        assert!(text.contains(r#"ollamamq_backend_free_slots{backend="127.0.0.1:9"} 0"#));
    }

    /// A backend that reports 2 slots runs two requests for the same
    /// (unlisted) model at once; with detection off it runs one at a time.
    #[tokio::test]
    async fn detected_slots_allow_parallel_requests() {
        async fn run(detect: bool) -> usize {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let url = format!("http://{}", listener.local_addr().expect("addr"));
            tokio::spawn(async move {
                let app = axum::Router::new()
                    .route(
                        "/api/tags",
                        axum::routing::get(|| async {
                            axum::Json(serde_json::json!({ "models": [{ "name": "llama3" }] }))
                        }),
                    )
                    .route(
                        "/api/chat",
                        axum::routing::post(|| async {
                            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                            axum::Json(serde_json::json!({ "done": true }))
                        }),
                    );
                let _ = axum::serve(listener, app).await;
            });
            let state = new_test_state(vec![url]);
            arm_ollama_backend(&state, None);
            state.backends.lock_or_recover()[0].detected_slots = Some(2);
            state
                .detect_slots
                .store(detect, std::sync::atomic::Ordering::Relaxed);
            let _a = enqueue_llama_chat(&state, HeaderMap::new());
            let _b = enqueue_llama_chat(&state, HeaderMap::new());
            let worker = tokio::spawn(run_worker(state.clone()));
            let mut peak = 0;
            for _ in 0..40 {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                peak = peak.max(state.backends.lock_or_recover()[0].active_requests);
            }
            worker.abort();
            peak
        }
        assert_eq!(run(true).await, 2, "both slots used");
        assert_eq!(run(false).await, 1, "configured cap of 1");
    }

    #[test]
    fn olla_prefixes_are_stripped() {
        assert_eq!(strip_olla_prefix("/olla/openai/v1/chat/completions"), "/v1/chat/completions");
        assert_eq!(strip_olla_prefix("/olla/proxy/api/chat"), "/api/chat");
        assert_eq!(strip_olla_prefix("/olla/openaix/v1/models"), "/olla/openaix/v1/models");
        assert_eq!(strip_olla_prefix("/v1/models"), "/v1/models");
    }

    async fn call(state: &Arc<AppState>, path: &'static str, body: &'static [u8]) -> axum::response::Response {
        proxy_handler(
            State(state.clone()),
            ConnectInfo("127.0.0.1:41040".parse().expect("addr")),
            Method::POST,
            HeaderMap::new(),
            axum::extract::OriginalUri(path.parse().expect("uri")),
            Bytes::from_static(body),
        )
        .await
        .into_response()
    }

    /// Olla paths reach the same handling as the plain route (seen through
    /// the immediate 404 for a hidden model), but unknown routes behind the
    /// prefix stay closed.
    #[tokio::test]
    async fn olla_paths_route_like_plain_ones() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        state.backends.lock_or_recover()[0].available_models = Arc::new(set(&["llama3"]));
        hide_models(&state, "llama3", vec![]);
        let res = call(&state, "/olla/openai/v1/chat/completions", br#"{"model":"llama3"}"#).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let b = axum::body::to_bytes(res.into_body(), usize::MAX).await.expect("body");
        let v: serde_json::Value = serde_json::from_slice(&b).expect("json");
        assert_eq!(v["error"]["code"], "model_not_found", "routed as /v1/chat/completions");

        let res = call(&state, "/olla/proxy/admin/models", b"").await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert!(state.queues.lock_or_recover().is_empty());
    }

    /// An alias is resolved before routing: the backend receives the target
    /// model name, and the rest of the body is untouched.
    #[tokio::test]
    async fn model_alias_rewrites_the_request() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        let aliases: std::collections::BTreeMap<String, String> =
            [("Codex-Auto-Review".to_string(), "llama3".to_string())].into_iter().collect();
        state.set_model_aliases(Some(&aliases));

        let out = state.apply_model_alias(Bytes::from_static(
            br#"{"model":"codex-auto-review","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
        ));
        let v: serde_json::Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(v["model"], "llama3");
        assert_eq!(v["messages"][0]["content"], "hi");
        assert_eq!(v["stream"], true);

        // Not an alias: the exact bytes pass through.
        let body = Bytes::from_static(br#"{"model":"phi4",  "x":1}"#);
        assert_eq!(state.apply_model_alias(body.clone()), body);
    }

    /// An aliased request reaches the backend as valid JSON even though the
    /// rewritten body is longer than the client's `Content-Length`.
    #[tokio::test]
    async fn aliased_request_is_forwarded_with_its_new_length() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let app = axum::Router::new()
                .route(
                    "/api/tags",
                    axum::routing::get(|| async {
                        axum::Json(serde_json::json!({ "models": [{ "name": "a-longer-model-name" }] }))
                    }),
                )
                .route(
                    "/api/chat",
                    axum::routing::post(|body: Bytes| async move {
                        match serde_json::from_slice::<serde_json::Value>(&body) {
                            Ok(v) => (StatusCode::OK, v["model"].to_string()),
                            Err(e) => (StatusCode::BAD_REQUEST, e.to_string()),
                        }
                    }),
                );
            let _ = axum::serve(listener, app).await;
        });
        let state = new_test_state(vec![url]);
        arm_ollama_backend(&state, None);
        let aliases: std::collections::BTreeMap<String, String> =
            [("short".to_string(), "a-longer-model-name".to_string())].into_iter().collect();
        state.set_model_aliases(Some(&aliases));
        let worker = tokio::spawn(run_worker(state.clone()));

        let body = Bytes::from_static(br#"{"model":"short","messages":[]}"#);
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::CONTENT_LENGTH, body.len().into());
        headers.insert("x-user-id", "alias-test".parse().expect("header value"));
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            proxy_handler(
                State(state.clone()),
                ConnectInfo("127.0.0.1:41031".parse().expect("addr")),
                Method::POST,
                headers,
                axum::extract::OriginalUri("/api/chat".parse().expect("uri")),
                body,
            ),
        )
        .await
        .expect("answered")
        .into_response();
        let status = res.status();
        let text = axum::body::to_bytes(res.into_body(), usize::MAX).await.expect("body");
        worker.abort();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&text));
        assert_eq!(&text[..], br#""a-longer-model-name""#);
    }

    /// Without a key or X-User-ID a request is `ip-<address>`; with
    /// `ip_identity` off it shares `anonymous`.
    #[tokio::test]
    async fn ip_identity_for_unidentified_requests() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        state.backends.lock_or_recover()[0].available_models = Arc::new(set(&["llama3"]));
        hide_models(&state, "llama3", vec![]);
        let _ = call(&state, "/api/chat", br#"{"model":"llama3"}"#).await;
        state
            .ip_identity
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let _ = call(&state, "/api/chat", br#"{"model":"llama3"}"#).await;
        let text = crate::metrics::render(&state);
        assert!(text.contains(r#"client="ip-127.0.0.1",model="llama3",endpoint="/api/chat",outcome="hidden_model"} 1"#), "{text}");
        assert!(text.contains(r#"client="anonymous",model="llama3",endpoint="/api/chat",outcome="hidden_model"} 1"#), "{text}");
    }

    // -- Scheduler-tier integration tests ------------------------------------
    //
    // Each test stands up tiny local HTTP mocks (real TCP listeners on
    // 127.0.0.1:0) that mimic what a backend reports to the health probes,
    // and pre-sets state.backends to exactly the state those probe answers
    // produce BEFORE spawning the worker: the first probe cycle runs
    // immediately and overwrites backend state via apply_probe, so the
    // scenario the scheduler sees is the same whether or not that cycle
    // lands before the selection. Selection is observed via state.backends.

    /// Tiny backend mock: for every accepted connection it reads the
    /// request line and answers by path —
    ///  - the listing paths named in `listing` → 200 + their body
    ///  - `/v1/models` / `/api/tags` NOT named there → that backend's own 404,
    ///    verbatim (a Laya-style server has no model-listing route at all)
    ///  - GET /api/v1/models → 200 + LM Studio native list, or 404 when None
    ///  - GET /api/ps → 200 + LM Studio-style rejection (remembered as a bad
    ///    endpoint by the health loop)
    ///  - anything else (the actual proxied POST) → 200 {"ok":true}, quickly
    ///
    /// Returns (URL, abort handle of the accept loop).
    async fn spawn_mock_listing(
        listing: &[(&str, &str)],
        native_models: Option<&str>,
    ) -> (String, tokio::task::AbortHandle) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listing: Vec<(String, String)> = listing
            .iter()
            .map(|(p, b)| (p.to_string(), b.to_string()))
            .collect();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock backend");
        let url = format!("http://{}", listener.local_addr().expect("mock local addr"));
        let native = native_models.map(|s| s.to_string());
        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break, // accept loop aborted
                };
                let listing = listing.clone();
                let native = native.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let n = match sock.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let path = request
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("");
                    let (status, body) = match path {
                        "/v1/models" | "/api/tags" => match listing
                            .iter()
                            .find(|(p, _)| *p == path)
                            .map(|(_, body)| body.clone())
                        {
                            Some(json) => ("200 OK", json),
                            None => ("404 Not Found", r#"{"detail":"Not Found"}"#.to_string()),
                        },
                        "/api/v1/models" => match &native {
                            Some(json) => ("200 OK", json.clone()),
                            None => ("404 Not Found", r#"{"detail":"Not Found"}"#.to_string()),
                        },
                        "/api/ps" => (
                            "200 OK",
                            r#"{"error":"Unexpected endpoint or method."}"#.to_string(),
                        ),
                        _ => ("200 OK", r#"{"ok":true}"#.to_string()),
                    };
                    let response = format!(
                        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        status,
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        (url, handle.abort_handle())
    }

    async fn spawn_mock_backend(
        v1_models: &str,
        native_models: Option<&str>,
    ) -> (String, tokio::task::AbortHandle) {
        spawn_mock_listing(
            &[
                ("/v1/models", v1_models),
                // What a non-Ollama backend answers Ollama's listing route with
                // (the health loop remembers it as a bad endpoint).
                ("/api/tags", r#"{"error":"Unexpected endpoint or method."}"#),
            ],
            native_models,
        )
        .await
    }

    /// A Laya-style backend: it serves `/v1/systemone` and answers nothing that
    /// identifies its models, so BOTH listing routes get its own 404.
    async fn spawn_mock_backend_without_listing() -> (String, tokio::task::AbortHandle) {
        spawn_mock_listing(&[], None).await
    }

    /// LM Studio backend scenario shared by the tier tests: ovisocr2 loaded,
    /// qwen listed but not loaded (probe answers that reproduce exactly this
    /// state, loaded_state_known = true).
    const OVISOCR2_V1_MODELS: &str =
        r#"{"data":[{"id":"qwen3.8-27b"},{"id":"ovisocr2@q4_k_m"}]}"#;
    const OVISOCR2_NATIVE: &str = r#"{"models":[{"key":"ovisocr2@q4_k_m","display_name":"OvisOCR2","loaded_instances":[{"id":"ovisocr2@q4_k_m"}]}]}"#;
    /// OpenAI-style list for the second backend in each scenario.
    const QWEN_V1_MODELS: &str = r#"{"data":[{"id":"qwen3.8-27b"}]}"#;

    /// Marks `b` as an online LM Studio backend with the given available /
    /// loaded model sets and native model list — exactly the state the
    /// matching mock's probe answers produce.
    fn lmstudio_backend_state(
        b: &mut BackendStatus,
        available: &[&str],
        loaded: &[&str],
        native: Vec<LmModelInfo>,
    ) {
        b.is_online = true;
        b.api_type = BackendApiType::Both;
        b.available_models = Arc::new(set(available));
        b.loaded_models = Arc::new(set(loaded));
        b.loaded_state_known = true;
        b.lmstudio = true;
        b.native_models = Arc::new(native);
    }

    /// One LM Studio native-list entry with a loaded instance (id == key).
    fn lm_native_loaded(key: &str, display: &str) -> Vec<LmModelInfo> {
        vec![LmModelInfo {
            key: key.to_string(),
            display_name: Some(display.to_string()),
            loaded_instance_ids: vec![key.to_string()],
        }]
    }

    /// Two-backend test AppState (same shape as the one in
    /// `stuck_request_fails_fast_with_503`).
    fn new_test_state(urls: Vec<String>) -> Arc<AppState> {
        new_test_state_with_budget(urls, 512 * 1024 * 1024)
    }

    fn new_test_state_with_budget(urls: Vec<String>, max_queued_bytes: u64) -> Arc<AppState> {
        let backends = urls.into_iter().map(|u| (u, None)).collect();
        Arc::new(AppState::new(
            backends,
            5,     // request timeout
            86400, // load keep alive
            1,     // stuck_timeout: fail fast after 1s
            1,     // max concurrent per backend
            "appconf.yaml".into(),
            Vec::new(),
            crate::reqlog::RequestLogger::disabled(),
            65_536,
            max_queued_bytes,
        ))
    }

    /// Enqueue one `POST /v1/chat/completions` request for `qwen3.8-27b`
    /// under user "tester" (full Task construction copied from
    /// `stuck_request_fails_fast_with_503`). Returns the receiver — keep it
    /// alive for the test's duration so the worker doesn't drop the request
    /// as client-gone.
    fn enqueue_qwen_request(state: &Arc<AppState>) -> mpsc::Receiver<ResponsePart> {
        let (tx, rx) = mpsc::channel(32);
        let body = Bytes::from(r#"{"model":"qwen3.8-27b","messages":[]}"#);
        state
            .queued_bytes
            .fetch_add(body.len() as u64, std::sync::atomic::Ordering::Relaxed);
        state
            .queues
            .lock_or_recover()
            .entry("tester".to_string())
            .or_default()
            .push_back(Task {
                method: Method::POST,
                user: "127.0.0.1:41000".to_string(),
                path: "/v1/chat/completions".into(),
                headers: HeaderMap::new(),
                body,
                responder: tx,
                requested_model: Some("qwen3.8-27b".to_string()),
                stuck_warned: false,
                cache_passes: 0,
                queued_at: std::time::Instant::now(),
                key_label: None,
                session_key: None,
            });
        rx
    }

    /// Poll state.backends every 100 ms (10 s overall) until exactly one
    /// backend has taken the single enqueued request: in-flight
    /// (`active_requests == 1`) or completed (`processed_count == 1`). The
    /// mock answers instantly, so the in-flight window is shorter than the
    /// poll interval; the stable completion counter is the reliable signal.
    /// Returns the backend index that took the request.
    async fn wait_for_picked_backend(state: &Arc<AppState>) -> Option<usize> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let signals: Vec<usize> = state
                    .backends
                    .lock_or_recover()
                    .iter()
                    .map(|b| b.active_requests + b.processed_count)
                    .collect();
                let total: usize = signals.iter().sum();
                if total == 1 {
                    // Only one request exists, so the signal is on one backend.
                    return signals.into_iter().position(|s| s == 1);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Wait until the picked backend has completed the request
    /// (`processed_count == 1` is stable; the in-flight counter is back to 0).
    async fn wait_for_completion(state: &Arc<AppState>, idx: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let (processed, active) = {
                    let b = &state.backends.lock_or_recover()[idx];
                    (b.processed_count, b.active_requests)
                };
                if processed == 1 && active == 0 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("picked backend never completed the request");
    }

    /// Tier 1: an eligible backend where the requested model is LOADED wins
    /// over an eligible backend that only lists it (routing there would
    /// evict the other's loaded model).
    #[tokio::test]
    async fn tier1_routes_to_backend_with_model_loaded() {
        // backend0: LM Studio with ovisocr2 loaded; backend1: LM Studio
        // with qwen loaded.
        let mock0 = spawn_mock_backend(OVISOCR2_V1_MODELS, Some(OVISOCR2_NATIVE)).await;
        let mock1 = spawn_mock_backend(
            QWEN_V1_MODELS,
            Some(
                r#"{"models":[{"key":"unsloth/qwen3.8-27b@q8_0","display_name":"Qwen3.8 27B","loaded_instances":[{"id":"unsloth/qwen3.8-27b@q8_0"}]}]}"#,
            ),
        )
        .await;

        let state = new_test_state(vec![mock0.0.clone(), mock1.0.clone()]);

        // Same state the mocks' probe answers produce: the immediate first
        // health cycle may run and apply them without changing the scenario.
        {
            let mut backends = state.backends.lock_or_recover();
            lmstudio_backend_state(
                &mut backends[0],
                &["qwen3.8-27b", "ovisocr2@q4_k_m"],
                &["ovisocr2@q4_k_m"],
                lm_native_loaded("ovisocr2@q4_k_m", "OvisOCR2"),
            );
            lmstudio_backend_state(
                &mut backends[1],
                &["qwen3.8-27b"],
                &["unsloth/qwen3.8-27b@q8_0"],
                lm_native_loaded("unsloth/qwen3.8-27b@q8_0", "Qwen3.8 27B"),
            );
        }

        let _rx = enqueue_qwen_request(&state);
        let worker = tokio::spawn(run_worker(state.clone()));

        // qwen is loaded on backend1 (tier 1); backend0 would have to evict
        // ovisocr2 — the request must go to backend1.
        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 1);
        wait_for_completion(&state, picked).await;
        {
            let backends = state.backends.lock_or_recover();
            assert_eq!(backends[1].processed_count, 1);
            assert_eq!(backends[0].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// Tier 1: a backend that does not report loaded state at all
    /// (vLLM-style: no /api/ps or /api/v1/models) counts as READY when it
    /// lists the requested model, and is preferred over a backend that
    /// would have to evict a loaded model.
    #[tokio::test]
    async fn tier1_treats_unknown_loaded_state_as_ready() {
        // backend0: LM Studio with ovisocr2 loaded; backend1: vLLM-style
        // (404 on the native list, so loaded state stays unknown).
        let mock0 = spawn_mock_backend(OVISOCR2_V1_MODELS, Some(OVISOCR2_NATIVE)).await;
        let mock1 = spawn_mock_backend(QWEN_V1_MODELS, None).await;

        let state = new_test_state(vec![mock0.0.clone(), mock1.0.clone()]);
        {
            let mut backends = state.backends.lock_or_recover();
            lmstudio_backend_state(
                &mut backends[0],
                &["qwen3.8-27b", "ovisocr2@q4_k_m"],
                &["ovisocr2@q4_k_m"],
                lm_native_loaded("ovisocr2@q4_k_m", "OvisOCR2"),
            );
            // vLLM-style: lists the model, reports no loaded state.
            backends[1].is_online = true;
            backends[1].api_type = BackendApiType::OpenAi;
            backends[1].available_models = Arc::new(set(&["qwen3.8-27b"]));
            backends[1].loaded_models = Arc::default();
            backends[1].loaded_state_known = false;
            backends[1].lmstudio = false;
            backends[1].native_models = Arc::default();
        }

        let _rx = enqueue_qwen_request(&state);
        let worker = tokio::spawn(run_worker(state.clone()));

        // Unknown loaded state implies ready: backend1 is tier 1 even though
        // it cannot prove qwen is resident.
        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 1);
        wait_for_completion(&state, picked).await;
        {
            let backends = state.backends.lock_or_recover();
            assert_eq!(backends[1].processed_count, 1);
            assert_eq!(backends[0].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// Tier 2: a backend that reports loaded state but has nothing loaded
    /// (cold — loading the requested model evicts nothing) is preferred
    /// over an eligible backend with a different model loaded.
    #[tokio::test]
    async fn tier2_prefers_cold_backend_over_loaded_other() {
        // backend0: LM Studio with ovisocr2 loaded; backend1: cold LM Studio
        // (native list is empty: state known, nothing loaded).
        let mock0 = spawn_mock_backend(OVISOCR2_V1_MODELS, Some(OVISOCR2_NATIVE)).await;
        let mock1 = spawn_mock_backend(QWEN_V1_MODELS, Some(r#"{"models":[]}"#)).await;

        let state = new_test_state(vec![mock0.0.clone(), mock1.0.clone()]);
        {
            let mut backends = state.backends.lock_or_recover();
            lmstudio_backend_state(
                &mut backends[0],
                &["qwen3.8-27b", "ovisocr2@q4_k_m"],
                &["ovisocr2@q4_k_m"],
                lm_native_loaded("ovisocr2@q4_k_m", "OvisOCR2"),
            );
            lmstudio_backend_state(&mut backends[1], &["qwen3.8-27b"], &[], Vec::new());
        }

        let _rx = enqueue_qwen_request(&state);
        let worker = tokio::spawn(run_worker(state.clone()));

        // No backend has qwen loaded; the cold backend1 is tier 2 and wins
        // over backend0, whose ovisocr2 would be evicted.
        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 1);
        wait_for_completion(&state, picked).await;
        {
            let backends = state.backends.lock_or_recover();
            assert_eq!(backends[1].processed_count, 1);
            assert_eq!(backends[0].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// Tier 3: when every eligible backend has a different model loaded and
    /// none is cold or ready, on-demand loading (which may evict the loaded
    /// model) is preserved as the last resort. Offline backends never enter
    /// the eligible set.
    #[tokio::test]
    async fn tier3_last_resort_on_demand_load() {
        let mock0 = spawn_mock_backend(OVISOCR2_V1_MODELS, Some(OVISOCR2_NATIVE)).await;
        // A port that was just closed: the "offline" backend never answers a
        // probe, so the immediate first health cycle keeps it offline.
        let offline_url = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind temp listener");
            let addr = l.local_addr().expect("temp addr");
            drop(l);
            format!("http://{}", addr)
        };

        let state = new_test_state(vec![mock0.0.clone(), offline_url]);
        {
            let mut backends = state.backends.lock_or_recover();
            lmstudio_backend_state(
                &mut backends[0],
                &["qwen3.8-27b", "ovisocr2@q4_k_m"],
                &["ovisocr2@q4_k_m"],
                lm_native_loaded("ovisocr2@q4_k_m", "OvisOCR2"),
            );
            lmstudio_backend_state(
                &mut backends[1],
                &["qwen3.8-27b", "ovisocr2@q4_k_m"],
                &["ovisocr2@q4_k_m"],
                lm_native_loaded("ovisocr2@q4_k_m", "OvisOCR2"),
            );
            backends[1].is_online = false; // not eligible
        }

        let _rx = enqueue_qwen_request(&state);
        let worker = tokio::spawn(run_worker(state.clone()));

        // Only backend0 is eligible and it has ovisocr2 loaded — qwen is
        // served there via on-demand load (last-resort tier), not dropped.
        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 0);
        wait_for_completion(&state, picked).await;
        {
            let backends = state.backends.lock_or_recover();
            assert_eq!(backends[0].processed_count, 1);
            assert_eq!(backends[1].processed_count, 0);
            assert!(!backends[1].is_online);
        }

        worker.abort();
        mock0.1.abort();
    }

    /// Accept connections and never answer them — a backend that is reachable
    /// but black-holed, the case a probe must not wait out at the inference
    /// timeout. Returns (url, abort handle of the accept loop).
    async fn spawn_black_holed_backend() -> (String, tokio::task::AbortHandle) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind black-holed backend");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let handle = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock); // hold the connection open, answer nothing
            }
        });
        (url, handle.abort_handle())
    }

    /// Read a finished response's JSON body.
    async fn response_json(res: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("read response body");
        serde_json::from_slice(&bytes).expect("response is JSON")
    }

    #[test]
    fn fair_share_decays_and_charges_every_dispatch() {
        let now = std::time::Instant::now();
        let mut u = FairShare::new(now);
        assert_eq!(u.value_at(now), 0.0);

        u.charge(now);
        assert!((u.value_at(now) - 1.0).abs() < 1e-9);
        // One half-life on, that unit of load counts half.
        let half = now + FAIR_SHARE_HALF_LIFE;
        assert!((u.value_at(half) - 0.5).abs() < 1e-9);

        // `touch` marks the user as seen without adding load.
        let mut v = FairShare::new(now);
        v.charge(now);
        v.touch(half);
        assert!((v.value_at(half) - 0.5).abs() < 1e-9);
        assert_eq!(v.last_seen(), half);
    }

        #[test]
    fn slot_reserve_privileges_only_a_recent_session() {
        let k = key("thread", "m");
        let other = key("other", "m");
        let now = std::time::Instant::now();
        let entry = |at: std::time::Instant, gap_max: f64, chain: u16| SlotReserve {
            at,
            gap_max_ms: gap_max,
            chain,
        };
        let reserved =
            HashMap::from([(k.clone(), entry(now - std::time::Duration::from_secs(3), 0.0, 0))]);
        // A session that finished a turn inside the window goes first.
        assert!(slot_reserved(Some(&k), &reserved, now));
        // Everything else is ordinary traffic.
        assert!(!slot_reserved(Some(&other), &reserved, now));
        assert!(!slot_reserved(None, &reserved, now));
        // Past the window the session lost its claim on the slot.
        assert!(!slot_reserved(
            Some(&k),
            &reserved,
            now + std::time::Duration::from_secs(5)
        ));
        // A session whose worst recent pause was 20 s stays privileged 12 s
        // after its last turn (window 30 s); the unmeasured default is 5 s.
        let slow = HashMap::from([(
            k.clone(),
            entry(now - std::time::Duration::from_secs(12), 20_000.0, 0),
        )]);
        assert!(slot_reserved(Some(&k), &slow, now));
        // ... and a snappy one (worst pause 0.1 s -> 2 s floor) is not.
        let fast = HashMap::from([(
            k.clone(),
            entry(now - std::time::Duration::from_secs(3), 100.0, 0),
        )]);
        assert!(!slot_reserved(Some(&k), &fast, now));
        // The consecutive-win cap forces an ordinary round at the limit.
        let chained = HashMap::from([(
            k.clone(),
            entry(now - std::time::Duration::from_secs(1), 0.0, SLOT_RESERVE_CHAIN),
        )]);
        assert!(!slot_reserved(Some(&k), &chained, now));
    }

    #[test]
    fn reservation_bookkeeping_follows_the_observed_gaps() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        let k = key("thread", "m");
        // Scripted clock: completions land one second apart, so every
        // observed turnaround is 1 s until the scripted 20 s pause.
        let c = std::time::Instant::now();
        state.slot_reserve.lock_or_recover().insert(
            k.clone(),
            SlotReserve {
                at: c,
                gap_max_ms: 0.0,
                chain: 0,
            },
        );
        for turn in 1..=3u64 {
            note_follow_up(state.as_ref(), &k, c + std::time::Duration::from_secs(turn));
            let mut table = state.slot_reserve.lock_or_recover();
            let e = table.get_mut(&k).unwrap();
            assert_eq!(e.chain, turn as u16, "reserved win each turn");
            assert!((e.gap_max_ms - 1_000.0).abs() < 40.0);
            e.at = c + std::time::Duration::from_secs(turn); // the completion
        }
        // A 20 s pause exceeds the 2 s floor window: chain resets and the
        // decaying max adopts it (window would now be 30 s, clamped).
        note_follow_up(
            state.as_ref(),
            &k,
            c + std::time::Duration::from_secs(23),
        );
        let e: SlotReserve = state.slot_reserve.lock_or_recover()[&k];
        assert_eq!(e.chain, 0, "a dispatch outside the window breaks the chain");
        assert!((e.gap_max_ms - 20_000.0).abs() < 60.0);
        assert_eq!(reserve_window(e.gap_max_ms), SLOT_RESERVE_MAX);
    }

    #[test]
    fn reserve_window_tracks_the_sessions_own_turnaround() {
        // Nothing measured: the reference-fleet default.
        assert_eq!(reserve_window(0.0), SLOT_RESERVE);
        // 1.5x the decaying worst gap, clamped to the bounds.
        assert_eq!(reserve_window(2_000.0), std::time::Duration::from_secs(3));
        assert_eq!(reserve_window(1.0), SLOT_RESERVE_MIN);
        assert_eq!(reserve_window(60_000.0), SLOT_RESERVE_MAX);
    }

    #[test]
    fn cache_hit_pct_is_the_skipped_share() {
        assert_eq!(cache_hit_pct(100, Some(25)), Some(75));
        assert_eq!(cache_hit_pct(100, Some(0)), Some(100));
        assert_eq!(cache_hit_pct(100, Some(100)), Some(0));
        assert_eq!(cache_hit_pct(100, Some(150)), Some(0)); // clamped, never negative
        assert_eq!(cache_hit_pct(0, Some(0)), None);
        assert_eq!(cache_hit_pct(100, None), None); // server reported nothing
    }

    #[test]
    fn cold_hold_only_holds_a_starved_cache_miss() {
        let k = key("thread", "m");
        let url = "http://127.0.0.1:9";
        let now = std::time::Instant::now();
        let ago = |secs: u64| now - std::time::Duration::from_secs(secs);
        let backends = |free: Option<u8>| {
            let mut b = BackendStatus::new(url.to_string(), None, Vec::new());
            b.kv_free_pct = free;
            vec![b]
        };
        let aff = HashMap::from([(
            k.clone(),
            Affinity {
                backend_url: url.to_string(),
                last_use: now,
            },
        )]);
        let empty_aff: HashMap<SessionKey, Affinity> = HashMap::new();
        // The candidate (fresh miss `hit`% at `at_cold`) plus `n_fresher`
        // conversations this backend served more recently.
        let memory = |hit: u8, at_cold: std::time::Instant, n_fresher: usize| {
            let mut m = HashMap::from([(
                (k.clone(), url.to_string()),
                ConvCache {
                    hit_pct: hit,
                    at: at_cold,
                    turns: 3,
                    inc_tok: None,
                },
            )]);
            for i in 0..n_fresher {
                m.insert(
                    (
                        key(&format!("other-{i}"), "m"),
                        url.to_string(),
                    ),
                    ConvCache {
                        hit_pct: 100,
                        at: now - std::time::Duration::from_secs(1 + i as u64 / 10),
                        turns: 5,
                        inc_tok: None,
                    },
                );
            }
            m
        };
        let fresh = |hit: u8| memory(hit, ago(3), 2);
        let empty_mem: HashMap<(SessionKey, String), ConvCache> = HashMap::new();

        // A fresh miss with two hotter conversations served since: hold.
        assert!(cold_hold(
            Some(&k),
            0,
            &aff,
            &fresh(10),
            &backends(Some(5)),
            now
        ));
        // No session identity, no memory, no affinity: nothing to hold.
        assert!(!cold_hold(None, 0, &aff, &fresh(10), &backends(Some(5)), now));
        assert!(!cold_hold(
            Some(&k),
            0,
            &aff,
            &empty_mem,
            &backends(Some(5)),
            now
        ));
        assert!(!cold_hold(
            Some(&k),
            0,
            &empty_aff,
            &fresh(10),
            &backends(Some(5)),
            now
        ));
        // Cache-hot conversation: it is the one the hold protects.
        assert!(!cold_hold(
            Some(&k),
            0,
            &aff,
            &memory(90, ago(3), 2),
            &backends(Some(5)),
            now
        ));
        // A first- or second-turn miss proves nothing about contention: no hold.
        {
            let mut m = fresh(10);
            m.get_mut(&(k.clone(), url.to_string())).unwrap().turns = 1;
            assert!(!cold_hold(Some(&k), 0, &aff, &m, &backends(Some(5)), now));
        }
        // Inside the grace window (fewer than CACHE_GRACE newer services): the
        // cache may still hold this conversation's context; never hold it.
        assert!(!cold_hold(
            Some(&k),
            0,
            &aff,
            &memory(10, ago(3), 1),
            &backends(Some(5)),
            now
        ));
        // No KV telemetry (plain Ollama): the pathology is unobservable.
        assert!(!cold_hold(
            Some(&k),
            0,
            &aff,
            &fresh(10),
            &backends(None),
            now
        ));
        // A fully free cache reports itself as such: room for this turn anyway.
        assert!(!cold_hold(
            Some(&k),
            0,
            &aff,
            &fresh(10),
            &backends(Some(100)),
            now
        ));
        // A stale measurement describes an unknown cache: never cold.
        assert!(!cold_hold(
            Some(&k),
            0,
            &aff,
            &memory(10, ago(600), 2),
            &backends(Some(5)),
            now
        ));
        // The yield budget bounds starvation: spent, the miss is dispatched.
        assert!(!cold_hold(
            Some(&k),
            CACHE_HOLD_PASSES,
            &aff,
            &fresh(10),
            &backends(Some(5)),
            now
        ));
        // A remembered backend that is gone holds nobody.
        let mut gone = backends(Some(5));
        gone[0].url = "http://elsewhere".into();
        assert!(!cold_hold(
            Some(&k),
            0,
            &aff,
            &fresh(10),
            &gone,
            now
        ));
        // KV budget: the backend can size its cache (200k tokens, 5% free =
        // 10k) and the conversation's measured increment decides, not just
        // "the cache is not empty".
        let mut budgeted = backends(Some(5));
        budgeted[0].loaded_ctx.insert("m".to_string(), 200_000);
        let with_inc = |inc: Option<u64>| {
            let mut m = fresh(10);
            m.get_mut(&(k.clone(), url.to_string())).unwrap().inc_tok = inc;
            m
        };
        // 30k to refeed > 10k free: it would evict the resident set, hold.
        assert!(cold_hold(
            Some(&k),
            0,
            &aff,
            &with_inc(Some(30_000)),
            &budgeted,
            now
        ));
        // 5k < 10k free: it fits without eviction, no reason to hold even
        // though the cache is 95% full.
        assert!(!cold_hold(
            Some(&k),
            0,
            &aff,
            &with_inc(Some(5_000)),
            &budgeted,
            now
        ));
        // Unknown increment (server without prompt_eval): the pressure
        // fallback applies again.
        assert!(cold_hold(
            Some(&k),
            0,
            &aff,
            &with_inc(None),
            &budgeted,
            now
        ));
    }

    /// A served conversation's prefix-cache hit is recorded per
    /// (session, backend) from the server's own usage/timings, and a
    /// halogen-style `/metrics` gauge lands on the backend as `kv_free_pct` —
    /// the two signals `cold_hold` reads.
    #[tokio::test]
    async fn cache_hit_and_kv_pressure_are_recorded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let app = axum::Router::new()
                .route(
                    "/v1/models",
                    axum::routing::get(|| async {
                        axum::Json(serde_json::json!({ "data": [{ "id": "m" }] }))
                    }),
                )
                .route(
                    "/metrics",
                    axum::routing::get(|| async { "llamacpp:kv_cache_usage 0.95\n" }),
                )
                .route(
                    "/v1/chat/completions",
                    axum::routing::post(|| async {
                        axum::Json(serde_json::json!({
                            "choices": [{ "message": { "content": "hi" } }],
                            "usage": { "prompt_tokens": 100, "completion_tokens": 2 },
                            "timings": { "prompt_n": 10 },
                        }))
                    }),
                );
            let _ = axum::serve(listener, app).await;
        });
        let state = new_test_state(vec![url]);
        let worker = tokio::spawn(run_worker(state.clone()));

        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "thread-1".parse().expect("header value"));
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            proxy_handler(
                State(state.clone()),
                ConnectInfo("127.0.0.1:41040".parse().expect("addr")),
                Method::POST,
                headers,
                axum::extract::OriginalUri("/v1/chat/completions".parse().expect("uri")),
                Bytes::from_static(br#"{"model":"m","messages":[]}"#),
            ),
        )
        .await
        .expect("answered")
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        let _ = axum::body::to_bytes(res.into_body(), usize::MAX).await;

        // Both signals are populated asynchronously: the usage at response
        // end, the KV gauge on a probe round.
        let (hit, free) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let free = state.backends.lock_or_recover()[0].kv_free_pct;
                let hit = state
                    .conv_cache
                    .lock_or_recover()
                    .iter()
                    .find(|((k, _), _)| k.conversation == "thread-1")
                    .map(|(_, c)| c.hit_pct);
                if let (Some(h), Some(f)) = (hit, free) {
                    break (h, f);
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the cache signals were never recorded");
        assert_eq!(hit, 90, "10 of 100 prompt tokens evaluated");
        assert_eq!(free, 5, "0.95 used = 5% free");
        worker.abort();
    }

    #[test]
    fn fair_share_lets_a_past_heavy_user_back_in() {
        // The old ordering key was cumulative since startup, so a user with a
        // big history stayed behind a newcomer for thousands of requests.
        let now = std::time::Instant::now();
        let mut heavy = FairShare::new(now);
        for _ in 0..10 {
            heavy.charge(now);
        }
        let mut light = FairShare::new(now);
        light.charge(now);

        // Twenty minutes (four half-lives) later the heavy user has been
        // quiet while the light user keeps working: 10/16 vs 1/16 + 1.
        let later = now + std::time::Duration::from_secs(1200);
        light.charge(later);
        assert!(
            heavy.value_at(later) < light.value_at(later),
            "the formerly heavy user must get a turn: {} vs {}",
            heavy.value_at(later),
            light.value_at(later)
        );
    }

    #[tokio::test]
    async fn queue_depth_is_capped_per_user() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        // Fill one user's queue to the cap. No worker runs, so nothing drains.
        // The receivers are held for the test's duration: a dropped one makes
        // the task look client-gone.
        let mut keep_alive = Vec::new();
        {
            let mut queues = state.queues.lock_or_recover();
            let q = queues.entry("tester".to_string()).or_default();
            for _ in 0..MAX_QUEUED_PER_USER {
                let (tx, rx) = mpsc::channel(32);
                keep_alive.push(rx);
                q.push_back(Task {
                    method: Method::POST,
                    user: "127.0.0.1:41000".to_string(),
                    path: "/api/chat".into(),
                    headers: HeaderMap::new(),
                    body: Bytes::from_static(b"{}"),
                    responder: tx,
                    requested_model: None,
                    stuck_warned: false,
                    cache_passes: 0,
                    queued_at: std::time::Instant::now(),
                    key_label: None,
                    session_key: None,
                });
            }
        }

        let mut headers = HeaderMap::new();
        headers.insert("X-User-ID", "tester".parse().expect("header value"));
        let res = proxy_handler(
            State(state.clone()),
            ConnectInfo("127.0.0.1:41001".parse().expect("addr")),
            Method::POST,
            headers,
            axum::extract::OriginalUri("/api/chat".parse().expect("uri")),
            Bytes::from_static(br#"{"model":"llama3"}"#),
        )
        .await
        .into_response();

        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        // The queue is unchanged and the refusal is counted as a drop.
        assert_eq!(
            state.queues.lock_or_recover()["tester"].len(),
            MAX_QUEUED_PER_USER
        );
        assert_eq!(
            state.dropped_counts.lock_or_recover().get("tester").copied(),
            Some(1)
        );
        drop(keep_alive);
    }

    // -- Sticky routing (session affinity) ------------------------------------
    //
    // A conversation is remembered by the backend that served its previous turn
    // and prefers to go back there, so a long multi-turn prompt is not re-prefilled
    // somewhere that has no KV slot for it. A preference, never a pin.

    /// Turns sticky routing on with explicit settings (see `settings.apply`).
    fn sticky_on(state: &Arc<AppState>, mode: StickyMode, lag: u64) {
        state
            .sticky_mode
            .store(mode as u8, std::sync::atomic::Ordering::Relaxed);
        state
            .sticky_lag
            .store(lag, std::sync::atomic::Ordering::Relaxed);
    }

    fn key(id: &str, model: &str) -> SessionKey {
        SessionKey {
            conversation: id.to_string(),
            model: model.to_string(),
        }
    }

    #[test]
    fn session_tag_is_short_stable_and_distinct() {
        let a = key("p:abc123", "m");
        assert_eq!(a.tag(), a.tag(), "stable across calls");
        assert_eq!(a.tag().len(), 6, "six hex digits");
        assert!(a.tag().bytes().all(|b| b.is_ascii_hexdigit()));
        // The tag identifies the conversation, not the model or the whole key.
        assert_eq!(key("p:abc123", "other").tag(), a.tag());
        assert_ne!(key("p:zzz999", "m").tag(), a.tag(), "different conversation");
    }

    /// True once the conversation is remembered on `url` — polled, because the
    /// mapping is rewritten when the response finishes, not at dispatch.
    async fn affinity_is(state: &Arc<AppState>, k: &SessionKey, url: &str) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let hit = state
                    .affinity
                    .lock_or_recover()
                    .get(k)
                    .is_some_and(|a| a.backend_url == url);
                if hit {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or(false)
    }

    /// The conversation id used by the routing tests below, as the scheduler
    /// would derive it from the request's body.
    fn body_key(body: &str, model: &str) -> Option<SessionKey> {
        derive_session_key(
            StickyMode::Prompt,
            "/v1/chat/completions",
            &HeaderMap::new(),
            body.as_bytes(),
            Some(model),
            "tester",
        )
    }

    /// Enqueues `body` under user "tester" with the session key the proxy would
    /// have derived from it.
    fn enqueue_chat_with_key(
        state: &Arc<AppState>,
        model: &str,
        body: String,
        session_key: Option<SessionKey>,
    ) -> mpsc::Receiver<ResponsePart> {
        let (tx, rx) = mpsc::channel(32);
        let body = Bytes::from(body);
        state
            .queued_bytes
            .fetch_add(body.len() as u64, std::sync::atomic::Ordering::Relaxed);
        state
            .queues
            .lock_or_recover()
            .entry("tester".to_string())
            .or_default()
            .push_back(Task {
                method: Method::POST,
                user: "127.0.0.1:41000".to_string(),
                path: "/v1/chat/completions".into(),
                headers: HeaderMap::new(),
                body,
                responder: tx,
                requested_model: Some(model.to_string()),
                stuck_warned: false,
                cache_passes: 0,
                queued_at: std::time::Instant::now(),
                key_label: None,
                session_key,
            });
        rx
    }

    #[test]
    fn session_key_prefers_explicit_ids_and_fingerprints_the_prompt() {
        let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;

        // Off: no affinity at all, whatever the request looks like.
        assert!(
            derive_session_key(
                StickyMode::Off,
                "/v1/chat/completions",
                &HeaderMap::new(),
                body.as_bytes(),
                Some("m"),
                "tester",
            )
            .is_none()
        );

        // An explicit id (header or body) wins over the fingerprint in every
        // mode that looks at the body at all.
        let mut headers = HeaderMap::new();
        headers.insert("X-Session-Id", "thread-7".parse().unwrap());
        let k = derive_session_key(
            StickyMode::Prompt,
            "/v1/chat/completions",
            &headers,
            body.as_bytes(),
            Some("m"),
            "tester",
        )
        .unwrap();
        assert_eq!(k.conversation, "thread-7");
        assert_eq!(k.model, "m");
        let k = derive_session_key(
            StickyMode::Header,
            "/v1/chat/completions",
            &HeaderMap::new(),
            br#"{"model":"m","prompt_cache_key":"pc-1","prompt":"x"}"#,
            Some("m"),
            "tester",
        )
        .unwrap();
        assert_eq!(k.conversation, "pc-1");

        // `header` mode does not guess from the prompt, `prompt` mode does.
        assert!(
            derive_session_key(
                StickyMode::Header,
                "/v1/chat/completions",
                &HeaderMap::new(),
                body.as_bytes(),
                Some("m"),
                "tester",
            )
            .is_none()
        );
        let fp = derive_session_key(
            StickyMode::Prompt,
            "/v1/chat/completions",
            &HeaderMap::new(),
            body.as_bytes(),
            Some("m"),
            "tester",
        )
        .unwrap();
        assert!(fp.conversation.starts_with("p:"));

        // Identity mode pins a whole client id, and `identity` is not `prompt`.
        assert_eq!(
            derive_session_key(
                StickyMode::Identity,
                "/api/chat",
                &HeaderMap::new(),
                body.as_bytes(),
                Some("m"),
                "tester",
            )
            .unwrap()
            .conversation,
            "u:tester"
        );

        // Routes without a conversation to keep warm are never keyed.
        assert!(
            derive_session_key(
                StickyMode::Prompt,
                "/v1/embeddings",
                &headers,
                body.as_bytes(),
                Some("m"),
                "tester",
            )
            .is_none()
        );
    }

    #[test]
    fn prompt_fingerprint_ignores_formatting_and_leading_system_turn() {
        let mk = |body: String| body_key(&body, "m").unwrap();
        let base = mk(
            r#"{"model":"m","messages":[{"role":"user","content":"first question"},{"role":"assistant","content":"answer"}]}"#.into(),
        );
        // Same conversation, different serialization: whitespace and the order
        // the fields appear in must not change the key.
        let same = mk(
            r#"{  "messages" : [ { "content" : "first question", "role" : "user" },
                  {"role": "assistant", "content": "answer"} ], "model": "m" }"#.into(),
        );
        assert_eq!(base, same);

        // A boilerplate system turn in front is not part of the identity.
        let with_system = mk(
            r#"{"model":"m","messages":[{"role":"system","content":"You are helpful"},{"role":"user","content":"first question"},{"role":"assistant","content":"answer"}]}"#.into(),
        );
        assert_eq!(base, with_system);

        // The same thread two turns later — the reply and the new question are
        // appended, and the key must not move with them. This is the property the
        // whole feature rests on: hash anything that grows and every turn looks
        // like a new conversation.
        let turn2 = mk(
            r#"{"model":"m","messages":[{"role":"user","content":"first question"},{"role":"assistant","content":"answer"},{"role":"user","content":"and then?"}]}"#.into(),
        );
        assert_eq!(base, turn2);

        // Another conversation, another key; so is another model.
        assert_ne!(
            base,
            mk(r#"{"model":"m","messages":[{"role":"user","content":"totally different"}]}"#.into())
        );


        // Nothing to fingerprint: no key rather than a guess.
        assert!(body_key(r#"{"model":"m","stream":true}"#, "m").is_none());
        assert!(body_key("not json at all", "m").is_none());
    }

    #[test]
    fn affinity_prune_drops_stale_entries_and_backends_that_are_gone() {
        let state = new_test_state(vec!["http://127.0.0.1:11434".into()]);
        state
            .sticky_ttl
            .store(1800, std::sync::atomic::Ordering::Relaxed);
        state
            .sticky_max_entries
            .store(2, std::sync::atomic::Ordering::Relaxed);
        {
            let mut table = state.affinity.lock_or_recover();
            // Already past the TTL.
            table.insert(
                key("stale", "m"),
                Affinity {
                    backend_url: "http://127.0.0.1:11434".into(),
                    last_use: std::time::Instant::now()
                        - std::time::Duration::from_secs(1900),
                },
            );
            // Points at a backend the proxy no longer has.
            table.insert(
                key("ghost", "m"),
                Affinity {
                    backend_url: "http://127.0.0.1:9".into(),
                    last_use: std::time::Instant::now(),
                },
            );
            for id in ["fresh-b", "fresh-a"] {
                table.insert(
                    key(id, "m"),
                    Affinity {
                        backend_url: "http://127.0.0.1:11434".into(),
                        last_use: std::time::Instant::now(),
                    },
                );
            }
        }
        prune_affinity(&state);
        let table = state.affinity.lock_or_recover();
        assert_eq!(table.len(), 2, "past the cap the idlest entries go first");
        assert!(table.contains_key(&key("fresh-b", "m")));
        assert!(table.contains_key(&key("fresh-a", "m")));
    }

    /// The remembered backend keeps the conversation even though the other one
    /// is just as free — the whole point: reprefilling a 200k context costs far
    /// more than leaving capacity unused.
    #[tokio::test]
    async fn sticky_routing_keeps_a_conversation_on_its_backend() {
        let home = spawn_mock_backend(M_V1_MODELS, None).await; // A: remembered
        let peer = spawn_mock_backend(M_V1_MODELS, None).await; // B: equally free
        let state = new_test_state(vec![home.0.clone(), peer.0.clone()]);
        {
            let mut backends = state.backends.lock_or_recover();
            openai_backend_state(&mut backends[0], &["m"]);
            openai_backend_state(&mut backends[1], &["m"]);
        }
        sticky_on(&state, StickyMode::Prompt, 0);

        let body = r#"{"model":"m","messages":[{"role":"user","content":"long story"}]}"#.to_string();
        let k = body_key(&body, "m").unwrap();
        state.affinity.lock_or_recover().insert(
            k.clone(),
            Affinity {
                backend_url: home.0.clone(),
                last_use: std::time::Instant::now(),
            },
        );

        let _rx = enqueue_chat_with_key(&state, "m", body, Some(k));
        let worker = tokio::spawn(run_worker(state.clone()));
        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        // Round-robin from last_backend_idx = 0 would have chosen backend 1.
        assert_eq!(picked, 0);
        wait_for_completion(&state, picked).await;
        assert_eq!(state.backends.lock_or_recover()[1].processed_count, 0);

        worker.abort();
        home.1.abort();
        peer.1.abort();
    }

    /// A remembered backend that cannot take the request now (or is running
    /// more than `sticky_lag` more than the lightest eligible one) loses the
    /// conversation, and the mapping follows the move so the turn after it does
    /// not move twice. Busy backends are stated as counters here, which is what
    /// the picker reads; the end-to-end test above covers the live path.
    #[test]
    fn a_busy_sticky_backend_loses_the_conversation() {
        let state = new_test_state(vec![
            "http://127.0.0.1:11434".into(),
            "http://127.0.0.1:11435".into(),
        ]);
        state
            .sticky_ttl
            .store(1800, std::sync::atomic::Ordering::Relaxed);
        let k = key("thread", "m");
        state.affinity.lock_or_recover().insert(
            k.clone(),
            Affinity {
                backend_url: "http://127.0.0.1:11434".into(),
                last_use: std::time::Instant::now(),
            },
        );
        let backends: Vec<BackendStatus> = state.backends.lock_or_recover().clone();
        let eligible: Vec<usize> = vec![0, 1];

        // No slack allowed and the remembered backend is the busiest: move.
        state
            .sticky_lag
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let mut busy = backends.clone();
        busy[0].active_requests = 1;
        assert_eq!(
            pick_sticky_backend(&state, &k, &eligible, &busy, 0),
            None
        );

        // One slot of slack, same load: it stays home.
        state
            .sticky_lag
            .store(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            pick_sticky_backend(&state, &k, &eligible, &busy, 0),
            Some(0)
        );

        // The remembered backend no longer serves the model: not eligible.
        state
            .sticky_lag
            .store(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pick_sticky_backend(&state, &k, &[1], &backends, 0), None);

        // Remembered for longer than the TTL: forgotten.
        state
            .sticky_ttl
            .store(1, std::sync::atomic::Ordering::Relaxed);
        state
            .affinity
            .lock_or_recover()
            .get_mut(&k)
            .unwrap()
            .last_use = std::time::Instant::now() - std::time::Duration::from_secs(30);
        assert_eq!(
            pick_sticky_backend(&state, &k, &eligible, &backends, 0),
            None
        );

        // And a move is remembered: the next turn comes back to the new backend.
        record_affinity(&state, &k, "http://127.0.0.1:11435");
        state
            .sticky_ttl
            .store(1800, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            pick_sticky_backend(&state, &k, &eligible, &backends, 0),
            Some(1)
        );
    }

    #[tokio::test]
    async fn queued_body_bytes_are_capped() {
        let state = new_test_state_with_budget(vec!["http://127.0.0.1:9".into()], 1_000);

        let call = |body: &'static [u8]| {
            let state = state.clone();
            let mut headers = HeaderMap::new();
            headers.insert("X-User-ID", "tester".parse().expect("header value"));
            async move {
                proxy_handler(
                    State(state),
                    ConnectInfo("127.0.0.1:41002".parse().expect("addr")),
                    Method::POST,
                    headers,
                    axum::extract::OriginalUri("/api/chat".parse().expect("uri")),
                    Bytes::from_static(body),
                )
                .await
                .into_response()
            }
        };

        // Bodies already waiting take the budget close to its limit.
        state
            .queued_bytes
            .store(900, std::sync::atomic::Ordering::Relaxed);
        let res = call(&[b'x'; 200]).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            state
                .queued_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            900,
            "a refused request must not consume budget"
        );
        assert_eq!(
            state.dropped_counts.lock_or_recover().get("tester").copied(),
            Some(1)
        );

        // With the queues empty, a body larger than the whole budget is still
        // admitted — it waits for a backend instead of being refused, so an
        // oversized request can never lock the proxy out of progress. No
        // worker runs, so an admitted request simply never completes.
        state
            .queued_bytes
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let admitted =
            tokio::time::timeout(std::time::Duration::from_millis(300), call(&[b'y'; 4_000]))
                .await;
        assert!(
            admitted.is_err(),
            "an oversized body on an empty queue must be admitted, not refused"
        );
        assert_eq!(
            state
                .queued_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            4_000,
            "an admitted request must claim its body's budget"
        );
    }

    #[tokio::test]
    async fn idle_users_are_pruned_but_active_ones_kept() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        let now = std::time::Instant::now();
        let a_while_ago = now
            .checked_sub(std::time::Duration::from_secs(1))
            .expect("monotonic clock has a second of history");

        {
            let mut scores = state.fair_share.lock_or_recover();
            // Seen a while back, nothing queued → prunable.
            scores.insert(
                "stale".into(),
                FairShare {
                    score: 0.0,
                    updated: a_while_ago,
                },
            );
            // Seen just now → kept regardless.
            scores.insert("recent".into(), FairShare::new(now));
            // Seen a while back, but still has a request waiting → kept.
            scores.insert(
                "waiting".into(),
                FairShare {
                    score: 0.0,
                    updated: a_while_ago,
                },
            );
        }
        state
            .processed_counts
            .lock_or_recover()
            .insert("stale".into(), 7);
        state
            .user_ips
            .lock_or_recover()
            .insert("stale".into(), "127.0.0.1".parse().expect("ip"));
        let (tx, rx) = mpsc::channel(32);
        {
            state
                .queues
                .lock_or_recover()
                .entry("waiting".to_string())
                .or_default()
                .push_back(Task {
                    method: Method::POST,
                    user: "127.0.0.1:41000".to_string(),
                    path: "/api/chat".into(),
                    headers: HeaderMap::new(),
                    body: Bytes::from_static(b"{}"),
                    responder: tx,
                    requested_model: None,
                    stuck_warned: false,
                    cache_passes: 0,
                    queued_at: now,
                    key_label: None,
                    session_key: None,
                });
        }

        prune_idle_users(&state, std::time::Duration::from_millis(500));

        let scores = state.fair_share.lock_or_recover();
        assert!(!scores.contains_key("stale"), "idle user should be dropped");
        assert!(scores.contains_key("recent"), "recently seen user stays");
        assert!(
            scores.contains_key("waiting"),
            "a user with a queued request must never be dropped"
        );
        // The stale user's other bookkeeping goes with them...
        assert!(!state.processed_counts.lock_or_recover().contains_key("stale"));
        assert!(!state.user_ips.lock_or_recover().contains_key("stale"));
        // ...and the waiting user's request is still queued.
        assert_eq!(state.queues.lock_or_recover()["waiting"].len(), 1);
        drop(rx);
    }

    #[tokio::test]
    async fn model_listing_is_merged_across_backends() {
        let (url_a, mock_a) = spawn_mock_backend(
            r#"{"object":"list","data":[{"id":"qwen3.8-27b"},{"id":"shared"}]}"#,
            None,
        )
        .await;
        let (url_b, mock_b) = spawn_mock_backend(
            r#"{"object":"list","data":[{"id":"shared"},{"id":"ovisocr2@q4_k_m"}]}"#,
            None,
        )
        .await;
        let state = new_test_state(vec![url_a, url_b]);
        {
            let mut backends = state.backends.lock_or_recover();
            for b in backends.iter_mut() {
                b.is_online = true;
                b.api_type = BackendApiType::OpenAi;
            }
        }

        let res = aggregate_model_list(&state, "/v1/models")
            .await
            .expect("both backends answer");
        let v = response_json(res).await;

        // The first answering backend's envelope is kept...
        assert_eq!(v["object"], "list");
        // ...and every backend's models are visible at once, deduped, in
        // backend order. Proxying to one backend showed only half of these.
        let ids: Vec<&str> = v["data"]
            .as_array()
            .expect("data array")
            .iter()
            .map(|m| m["id"].as_str().expect("id"))
            .collect();
        assert_eq!(ids, vec!["qwen3.8-27b", "shared", "ovisocr2@q4_k_m"]);

        mock_a.abort();
        mock_b.abort();
    }

    /// How CLM / Kev answer `GET /v1/models` — the System One envelope, not
    /// OpenAI's `{"data":[{"id":…}]}`.
    const SYSTEMONE_V1_MODELS: &str =
        r#"{"models":[{"name":"shared"},{"name":"clm-latest","description":"decision model"}]}"#;

    #[tokio::test]
    async fn model_listing_merges_openai_and_system_one_envelopes() {
        let (url_openai, mock_openai) = spawn_mock_backend(
            r#"{"object":"list","data":[{"id":"a"},{"id":"shared"}]}"#,
            None,
        )
        .await;
        let (url_sysone, mock_sysone) = spawn_mock_backend(SYSTEMONE_V1_MODELS, None).await;
        let state = new_test_state(vec![url_openai, url_sysone]);
        {
            let mut backends = state.backends.lock_or_recover();
            backends[0].is_online = true;
            backends[0].api_type = BackendApiType::OpenAi;
            backends[1].is_online = true;
            backends[1].api_type = ApiSet::SYSTEMONE;
        }

        let v = response_json(
            aggregate_model_list(&state, "/v1/models")
                .await
                .expect("both envelopes merge into one list"),
        )
        .await;

        // The first answering backend's envelope is still the one reused...
        assert_eq!(v["object"], "list");
        // ...and both envelopes are in it, deduped by name with the first writer
        // winning. Before this, the System One backend contributed nothing at
        // all: its array is called `models`, not `data`.
        let ids: Vec<&str> = v["data"]
            .as_array()
            .expect("data array")
            .iter()
            .map(|m| m["id"].as_str().expect("id"))
            .collect();
        assert_eq!(ids, vec!["a", "shared", "clm-latest"]);
        // The foreign entry is only re-keyed (`name` → `id`); its own metadata
        // survives untouched, as every other entry's does.
        assert_eq!(v["data"][2]["description"], "decision model");
        assert!(v["data"][2].get("name").is_none());
        // The merged answer stays a single envelope, not both side by side.
        assert!(v.get("models").is_none());

        mock_openai.abort();
        mock_sysone.abort();
    }

    /// The ordering that regressed in P1: a System One backend configured FIRST
    /// answers `/v1/models` as `{"models":[{"name":…}]}`, and reusing *that*
    /// body as the answer envelope re-keyed the merged list — breaking OpenAI SDK
    /// clients on a route the README advertises as OpenAI-compatible. The answer
    /// envelope is fixed by the route, so it stays `data`/`id`.
    #[tokio::test]
    async fn v1_models_answers_in_openai_envelope_when_system_one_is_first() {
        let (url_sysone, mock_sysone) = spawn_mock_backend(SYSTEMONE_V1_MODELS, None).await;
        let (url_openai, mock_openai) = spawn_mock_backend(QWEN_V1_MODELS, None).await;
        let state = new_test_state(vec![url_sysone, url_openai]);
        {
            let mut backends = state.backends.lock_or_recover();
            backends[0].is_online = true;
            backends[0].api_type = ApiSet::SYSTEMONE;
            backends[1].is_online = true;
            backends[1].api_type = BackendApiType::OpenAi;
        }

        let v = response_json(
            aggregate_model_list(&state, "/v1/models")
                .await
                .expect("both backends answer"),
        )
        .await;

        let ids: Vec<&str> = v["data"]
            .as_array()
            .expect("data array — the first answerer must not re-key it")
            .iter()
            .map(|m| m["id"].as_str().expect("id"))
            .collect();
        assert_eq!(ids, vec!["shared", "clm-latest", "qwen3.8-27b"]);
        // One envelope, never both: the array the System One body reported its
        // listing under is dropped once it has been read into `data`.
        assert!(v.get("models").is_none());
        // The foreign entry keeps its own metadata through the re-key.
        assert_eq!(v["data"][1]["description"], "decision model");
        assert!(v["data"][1].get("name").is_none());

        mock_sysone.abort();
        mock_openai.abort();
    }

    #[tokio::test]
    async fn model_listing_skips_incompatible_and_offline_backends() {
        let (url_a, mock_a) = spawn_mock_backend(QWEN_V1_MODELS, None).await;
        let (url_b, mock_b) =
            spawn_mock_backend(r#"{"object":"list","data":[{"id":"never-listed"}]}"#, None).await;
        let state = new_test_state(vec![url_a, url_b]);
        {
            let mut backends = state.backends.lock_or_recover();
            backends[0].is_online = true;
            backends[0].api_type = BackendApiType::OpenAi;
            // Speaks a different dialect: must not be asked for /v1/models.
            backends[1].is_online = true;
            backends[1].api_type = BackendApiType::Ollama;
        }

        let v = response_json(
            aggregate_model_list(&state, "/v1/models")
                .await
                .expect("the OpenAI backend answers"),
        )
        .await;
        let ids: Vec<&str> = v["data"]
            .as_array()
            .expect("data array")
            .iter()
            .map(|m| m["id"].as_str().expect("id"))
            .collect();
        assert_eq!(ids, vec!["qwen3.8-27b"]);

        // With no compatible backend online there is nothing to merge, and the
        // caller falls back to the normal queued proxy path.
        {
            let mut backends = state.backends.lock_or_recover();
            backends[0].is_online = false;
        }
        assert!(aggregate_model_list(&state, "/v1/models").await.is_none());

        mock_a.abort();
        mock_b.abort();
    }

    /// Mark `b` as an online System One backend that serves `models` and cannot
    /// list them itself (what `kind: systemone` + a Laya server produce).
    fn systemone_backend_state(b: &mut BackendStatus, models: &[&str]) {
        b.is_online = true;
        b.api_type = ApiSet::SYSTEMONE;
        b.available_models = Arc::new(set(models));
    }

    /// Laya answers `GET /v1/models` with its own 404, so before this fallback
    /// the listing route had nothing to merge, returned `None`, and the request
    /// was proxied — handing the client Laya's 404. A model nobody can list is a
    /// model nobody sends, however well it routes, so the names the proxy already
    /// knows (`available_models`: declared in `appconf.yaml`, kept by the probe)
    /// are answered instead, in the envelope of the route being asked.
    #[tokio::test]
    async fn model_listing_falls_back_to_backend_state_when_nothing_answers() {
        let (url, mock) = spawn_mock_backend_without_listing().await;
        let state = new_test_state(vec![url]);
        {
            let mut backends = state.backends.lock_or_recover();
            systemone_backend_state(&mut backends[0], &["multilingual"]);
        }

        let res = aggregate_model_list(&state, "/v1/models")
            .await
            .expect("a known name is enough to answer");
        assert_eq!(res.status(), StatusCode::OK);
        let v = response_json(res).await;
        assert_eq!(v["data"], serde_json::json!([{"id": "multilingual"}]));
        // The route's envelope, not the other one — synthesising does not
        // re-key `/v1/models` into the System One shape.
        assert!(v.get("models").is_none());

        let res = aggregate_model_list(&state, "/api/tags")
            .await
            .expect("the same names answer /api/tags");
        assert_eq!(res.status(), StatusCode::OK);
        let v = response_json(res).await;
        assert_eq!(v["models"], serde_json::json!([{"name": "multilingual"}]));
        assert!(v.get("data").is_none());

        mock.abort();
    }

    /// The mixed deployment: Laya (no listing endpoint, configured first) next
    /// to a CLM that answers its own. Both must be visible, and CLM's rich entry
    /// must survive a same-named synthesised one — synthesised entries are
    /// appended after the live ones precisely so they can never shadow metadata
    /// (`description`, `release_date`, `size`, `digest`) that only the backend
    /// itself can report.
    #[tokio::test]
    async fn model_listing_keeps_live_entries_over_synthesised_ones() {
        let (url_laya, mock_laya) = spawn_mock_backend_without_listing().await;
        let (url_clm, mock_clm) = spawn_mock_backend(SYSTEMONE_V1_MODELS, None).await;
        let state = new_test_state(vec![url_laya, url_clm]);
        {
            let mut backends = state.backends.lock_or_recover();
            // `clm-latest` is a name Laya knows about too (shared deployment):
            // CLM's entry for it must win even though Laya is listed first.
            systemone_backend_state(&mut backends[0], &["clm-latest", "multilingual"]);
            systemone_backend_state(&mut backends[1], &["shared", "clm-latest"]);
        }

        let v = response_json(
            aggregate_model_list(&state, "/v1/models")
                .await
                .expect("the answering backend plus the silent one's names merge"),
        )
        .await;
        let ids: Vec<&str> = v["data"]
            .as_array()
            .expect("data array")
            .iter()
            .map(|m| m["id"].as_str().expect("id"))
            .collect();
        assert_eq!(ids, vec!["shared", "clm-latest", "multilingual"]);
        // No duplicates, and the live entry kept its own metadata.
        assert_eq!(ids.len(), ids.iter().collect::<HashSet<_>>().len());
        assert_eq!(v["data"][1]["description"], "decision model");
        // The synthesised entry carries nothing but the route's name field.
        assert_eq!(v["data"][2], serde_json::json!({"id": "multilingual"}));

        mock_laya.abort();
        mock_clm.abort();
    }

    /// Every backend failed the listing route: the answer is a synthesised 200,
    /// not `None` (which would let the request fall through to a backend's 404).
    #[tokio::test]
    async fn model_listing_synthesises_200_instead_of_falling_through() {
        let (url_a, mock_a) = spawn_mock_backend_without_listing().await;
        let (url_b, mock_b) = spawn_mock_backend_without_listing().await;
        let state = new_test_state(vec![url_a, url_b]);
        {
            let mut backends = state.backends.lock_or_recover();
            systemone_backend_state(&mut backends[0], &["multilingual"]);
            systemone_backend_state(&mut backends[1], &["clm-latest"]);
        }

        for (endpoint, key, field) in [
            ("/v1/models", "data", "id"),
            ("/api/tags", "models", "name"),
        ] {
            let res = aggregate_model_list(&state, endpoint)
                .await
                .expect("names are enough: no fall-through to a backend 404");
            assert_eq!(res.status(), StatusCode::OK, "{endpoint} must answer 200");
            let v = response_json(res).await;
            let names: Vec<&str> = v[key]
                .as_array()
                .unwrap_or_else(|| panic!("{endpoint} answers in its own {key} envelope"))
                .iter()
                .map(|m| m[field].as_str().expect(field))
                .collect();
            assert_eq!(names, vec!["multilingual", "clm-latest"]);
        }

        mock_a.abort();
        mock_b.abort();
    }

    /// The other side of the fallback, and the reason it is limited to the
    /// System One family: a backend of another family that cannot serve this
    /// route still contributes nothing, so an existing Ollama / LM Studio /
    /// OpenAI deployment keeps taking the live-body path (or the normal proxy
    /// fall-through) with byte-identical output.
    #[tokio::test]
    async fn model_listing_invents_nothing_for_an_incompatible_family() {
        let (url, mock) = spawn_mock_backend_without_listing().await;
        let state = new_test_state(vec![url]);
        {
            let mut backends = state.backends.lock_or_recover();
            backends[0].is_online = true;
            backends[0].api_type = BackendApiType::Ollama;
            backends[0].available_models = Arc::new(set(&["llama3"]));
        }

        // `/v1/models` is not an Ollama route: nothing is asked, nothing is
        // invented, and the caller falls back to the normal queued proxy path.
        assert!(aggregate_model_list(&state, "/v1/models").await.is_none());

        mock.abort();
    }

    #[tokio::test]
    async fn metadata_request_is_served_by_a_busy_backend() {
        let (url, mock) = spawn_mock_backend(QWEN_V1_MODELS, None).await;
        let state = new_test_state(vec![url]);
        {
            let mut backends = state.backends.lock_or_recover();
            backends[0].is_online = true;
            backends[0].api_type = BackendApiType::OpenAi;
            backends[0].available_models = Arc::new(set(&["qwen3.8-27b"]));
            // A generation holds the only slot (max_concurrent_per_backend = 1)
            // and a model load is running on top of it.
            backends[0].active_requests = 1;
        }
        state.control_ops.lock_or_recover().insert(
            0,
            crate::control::ControlOp {
                backend_idx: 0,
                action: crate::control::ControlAction::Load,
                requested: "qwen3.8-27b".into(),
                canonical: "qwen3.8-27b".into(),
                identifier: None,
                started: std::time::Instant::now(),
            },
        );

        // Queued first: a chat request that cannot run until the slot frees.
        let _chat_rx = enqueue_qwen_request(&state);
        // Queued behind it, same user: a metadata read.
        let (tx, mut rx) = mpsc::channel(32);
        state
            .queues
            .lock_or_recover()
            .entry("tester".to_string())
            .or_default()
            .push_back(Task {
                method: Method::GET,
                user: "127.0.0.1:41000".to_string(),
                path: "/v1/models".into(),
                headers: HeaderMap::new(),
                body: Bytes::new(),
                responder: tx,
                requested_model: None,
                stuck_warned: false,
                cache_passes: 0,
                queued_at: std::time::Instant::now(),
                key_label: None,
                session_key: None,
            });

        let worker = tokio::spawn(run_worker(state.clone()));

        // The metadata read is answered even though the backend is at capacity
        // AND control-busy — it never waits on the generation ahead of it.
        let part = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("metadata request was not served within 10s");
        match part {
            Some(ResponsePart::Status(status, _)) => assert_eq!(status, StatusCode::OK),
            other => panic!("expected a status part, got {:?}", other.is_some()),
        }

        {
            let backends = state.backends.lock_or_recover();
            // It never took the inference slot: still just the generation.
            assert_eq!(backends[0].active_requests, 1);
            // And it did not evict the chat request, which is still waiting.
            assert_eq!(state.queues.lock_or_recover()["tester"].len(), 1);
        }

        worker.abort();
        mock.abort();
    }

    #[tokio::test]
    async fn abandoned_requests_are_not_counted_as_throughput() {
        let (url, mock) = spawn_mock_backend(QWEN_V1_MODELS, None).await;
        let state = new_test_state(vec![url]);
        {
            let mut backends = state.backends.lock_or_recover();
            backends[0].is_online = true;
            backends[0].api_type = BackendApiType::OpenAi;
            backends[0].available_models = Arc::new(set(&["qwen3.8-27b"]));
        }

        // The client hangs up before the request is dispatched.
        drop(enqueue_qwen_request(&state));
        let worker = tokio::spawn(run_worker(state.clone()));

        let dropped = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if state
                    .dropped_counts
                    .lock_or_recover()
                    .get("tester")
                    .copied()
                    .unwrap_or(0)
                    > 0
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(dropped.is_ok(), "the abandoned request was never resolved");

        {
            let backends = state.backends.lock_or_recover();
            // It reached a backend, but no response was delivered — it is not
            // throughput. `processed_count` used to be bumped on every exit
            // path, including this one.
            assert_eq!(backends[0].processed_count, 0);
            assert_eq!(backends[0].active_requests, 0);
            // With nothing in flight the backend has no current model; it used
            // to keep displaying the last one it ever touched.
            assert!(backends[0].current_model.is_none());
        }
        // Leaving the queue returns the body's share of the memory budget.
        assert_eq!(
            state
                .queued_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );

        worker.abort();
        mock.abort();
    }

    #[tokio::test]
    async fn health_round_probes_backends_concurrently() {
        // Three black-holed backends: every endpoint burns the full probe
        // budget, so a sequential round would take three times as long.
        let mut urls = Vec::new();
        let mut aborts = Vec::new();
        for _ in 0..3 {
            let (url, abort) = spawn_black_holed_backend().await;
            urls.push(url);
            aborts.push(abort);
        }
        let state = new_test_state(urls);
        let client = reqwest::Client::builder()
            .build()
            .expect("build probe client");
        let probe_timeout = std::time::Duration::from_millis(200);

        let started = std::time::Instant::now();
        health_check_round(&state, &client, true, probe_timeout).await;
        let elapsed = started.elapsed();
        for a in aborts {
            a.abort();
        }

        // One backend's own endpoint probes stay sequential (~4 x 200 ms);
        // three backends in sequence would be ~2.4 s.
        assert!(
            elapsed < std::time::Duration::from_millis(1800),
            "health round took {:?} — backends look sequential",
            elapsed
        );
        for b in state.backends.lock_or_recover().iter() {
            assert!(!b.is_online, "black-holed backend must be marked offline");
        }
    }

    // -- Backend-pinning tests -------------------------------------------------
    //
    // A non-empty `backends:` list in the model config pins routing: requests
    // for that model may only go to the listed backends (strict — no fallback
    // to other online backends, 503 after stuck_timeout when none can serve).

    /// OpenAI-style mock model list for a single bare model id "m".
    const M_V1_MODELS: &str = r#"{"data":[{"id":"m"}]}"#;

    /// Marks `b` as an online OpenAI backend listing the given models (no
    /// loaded-state reporting) — exactly the state the matching mock's probe
    /// answers produce.
    fn openai_backend_state(b: &mut BackendStatus, available: &[&str]) {
        b.is_online = true;
        b.api_type = BackendApiType::OpenAi;
        b.available_models = Arc::new(set(available));
        b.loaded_models = Arc::default();
        b.loaded_state_known = false;
    }

    /// Enqueue one `POST /v1/chat/completions` request for `model` under user
    /// "tester" (same shape as enqueue_qwen_request). Returns the receiver —
    /// keep it alive for the test's duration.
    fn enqueue_chat_request(state: &Arc<AppState>, model: &str) -> mpsc::Receiver<ResponsePart> {
        let (tx, rx) = mpsc::channel(32);
        state
            .queues
            .lock()
            .unwrap()
            .entry("tester".to_string())
            .or_default()
            .push_back(Task {
                method: Method::POST,
                user: "127.0.0.1:41000".to_string(),
                path: "/v1/chat/completions".into(),
                headers: HeaderMap::new(),
                body: Bytes::from(format!(r#"{{"model":"{}","messages":[]}}"#, model)),
                responder: tx,
                requested_model: Some(model.to_string()),
                stuck_warned: false,
                cache_passes: 0,
                queued_at: std::time::Instant::now(),
                key_label: None,
                session_key: None,
            });
        rx
    }

    /// Append a model entry to the test state's model config (the pin under
    /// test). `backends` empty = the entry does NOT pin.
    fn pin_model(
        state: &Arc<AppState>,
        name: &str,
        identifier: Option<&str>,
        backends: Vec<String>,
    ) {
        state
            .model_config
            .lock()
            .unwrap()
            .push(crate::config::ModelConfig {
                name: name.to_string(),
                identifier: identifier.map(|s| s.to_string()),
                max_ctx: None,
                keep_alive: None,
                max_concurrent_requests: 1,
                backends,
            });
    }

    /// Two-backend test AppState with a per-backend cap high enough that a
    /// pre-busy backend (active_requests = 1) still has capacity for the
    /// enqueued request (same shape as new_test_state).
    fn new_test_state_cap(urls: Vec<String>, cap: u32) -> Arc<AppState> {
        let backends = urls.into_iter().map(|u| (u, None)).collect();
        Arc::new(AppState::new(
            backends,
            5,     // request timeout
            86400, // load keep alive
            1,     // stuck_timeout: fail fast after 1s
            cap,   // max concurrent per backend
            "appconf.yaml".into(),
            Vec::new(),
            crate::reqlog::RequestLogger::disabled(),
            65_536,
            512 * 1024 * 1024,
        ))
    }

    /// Wait until the single enqueued request has been COMPLETED by one of the
    /// backends (processed_count == 1) and return which one. Unlike
    /// wait_for_picked_backend this ignores pre-set active_requests counts.
    async fn wait_for_served_by(state: &Arc<AppState>) -> Option<usize> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let done = state
                    .backends
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|b| b.processed_count)
                    .collect::<Vec<_>>();
                if let Some(pos) = done.iter().position(|&c| c == 1) {
                    return pos;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .ok()
    }

    /// Pinning restricts eligibility: a model pinned to backend A must be
    /// served by A even when the unpinned min-active-requests selection would
    /// pick the freer backend B.
    #[tokio::test]
    async fn pin_restricts_eligibility() {
        let mock0 = spawn_mock_backend(M_V1_MODELS, None).await; // A: pinned
        let mock1 = spawn_mock_backend(M_V1_MODELS, None).await; // B: freer

        let state = new_test_state_cap(vec![mock0.0.clone(), mock1.0.clone()], 2);
        {
            let mut backends = state.backends.lock().unwrap();
            openai_backend_state(&mut backends[0], &["m"]);
            openai_backend_state(&mut backends[1], &["m"]);
            // B has fewer active requests: unpinned selection would pick B.
            backends[0].active_requests = 1;
        }
        pin_model(&state, "m", None, vec![mock0.0.clone()]);

        let _rx = enqueue_chat_request(&state, "m");
        let worker = tokio::spawn(run_worker(state.clone()));

        // The pin forces backend A despite B being freer.
        let served = wait_for_served_by(&state)
            .await
            .expect("no backend served the request within 10s");
        assert_eq!(served, 0);
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[0].processed_count, 1);
            assert_eq!(backends[1].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// A pin matches by identifier as well as name: entry name "x/y" with
    /// identifier "m" pins requests for model "m" to backend A only.
    #[tokio::test]
    async fn pin_matches_by_identifier() {
        let mock0 = spawn_mock_backend(M_V1_MODELS, None).await; // A: pinned
        let mock1 = spawn_mock_backend(M_V1_MODELS, None).await;

        let state = new_test_state(vec![mock0.0.clone(), mock1.0.clone()]);
        {
            let mut backends = state.backends.lock().unwrap();
            openai_backend_state(&mut backends[0], &["m"]);
            openai_backend_state(&mut backends[1], &["m"]);
        }
        pin_model(&state, "x/y", Some("m"), vec![mock0.0.clone()]);

        let _rx = enqueue_chat_request(&state, "m");
        let worker = tokio::spawn(run_worker(state.clone()));

        // Unpinned round-robin (last_backend_idx = 0) would pick backend 1;
        // the identifier pin forces backend A.
        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 0);
        wait_for_completion(&state, picked).await;
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[0].processed_count, 1);
            assert_eq!(backends[1].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// A pin URL matches by substring (case-insensitive), consistent with
    /// `control::config_targets` load targeting: a scheme-less `backends:`
    /// entry (as in appconf.yaml.example) routes as well as loads. The pin is
    /// derived from backend A's actual URL with the `http://` prefix
    /// stripped, so only A's full URL contains it.
    #[tokio::test]
    async fn pin_matches_by_substring_url() {
        let mock0 = spawn_mock_backend(M_V1_MODELS, None).await; // A: pinned
        let mock1 = spawn_mock_backend(M_V1_MODELS, None).await;

        let state = new_test_state(vec![mock0.0.clone(), mock1.0.clone()]);
        {
            let mut backends = state.backends.lock().unwrap();
            openai_backend_state(&mut backends[0], &["m"]);
            openai_backend_state(&mut backends[1], &["m"]);
        }
        // Substring pin: A's URL without the scheme (B is a different port).
        let pin_url = mock0.0.trim_start_matches("http://").to_string();
        assert!(
            !pin_url.is_empty()
                && pin_url != mock0.0
                && !mock1.0.contains(&pin_url),
            "pin must be a proper substring of A's URL only: pin={} a={} b={}",
            pin_url,
            mock0.0,
            mock1.0
        );
        pin_model(&state, "m", None, vec![pin_url]);

        let _rx = enqueue_chat_request(&state, "m");
        let worker = tokio::spawn(run_worker(state.clone()));

        let served = wait_for_served_by(&state)
            .await
            .expect("no backend served the request within 10s");
        assert_eq!(served, 0);
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[0].processed_count, 1);
            assert_eq!(backends[1].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// A model absent from the config is unpinned: both backends stay
    /// eligible and selection follows the existing min-active-requests rule
    /// (the freer backend B wins).
    #[tokio::test]
    async fn unpinned_model_unchanged() {
        let mock0 = spawn_mock_backend(M_V1_MODELS, None).await;
        let mock1 = spawn_mock_backend(M_V1_MODELS, None).await;

        let state = new_test_state_cap(vec![mock0.0.clone(), mock1.0.clone()], 2);
        {
            let mut backends = state.backends.lock().unwrap();
            openai_backend_state(&mut backends[0], &["m"]);
            openai_backend_state(&mut backends[1], &["m"]);
            // Backend A is busier: min-active selection must pick B.
            backends[0].active_requests = 1;
        }
        // No config entry for "m" at all (state was built with an empty list).

        let _rx = enqueue_chat_request(&state, "m");
        let worker = tokio::spawn(run_worker(state.clone()));

        let served = wait_for_served_by(&state)
            .await
            .expect("no backend served the request within 10s");
        assert_eq!(served, 1);
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[1].processed_count, 1);
            assert_eq!(backends[0].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// A config entry with an empty `backends:` list is NOT a pin: the model
    /// stays eligible on every backend (min-active selection picks the freer
    /// backend B; a spurious pin to nothing would 503 instead).
    #[tokio::test]
    async fn empty_backends_not_a_pin() {
        let mock0 = spawn_mock_backend(M_V1_MODELS, None).await;
        let mock1 = spawn_mock_backend(M_V1_MODELS, None).await;

        let state = new_test_state_cap(vec![mock0.0.clone(), mock1.0.clone()], 2);
        {
            let mut backends = state.backends.lock().unwrap();
            openai_backend_state(&mut backends[0], &["m"]);
            openai_backend_state(&mut backends[1], &["m"]);
            backends[0].active_requests = 1; // B is freer
        }
        pin_model(&state, "m", None, Vec::new()); // empty list → no pin

        let _rx = enqueue_chat_request(&state, "m");
        let worker = tokio::spawn(run_worker(state.clone()));

        let served = wait_for_served_by(&state)
            .await
            .expect("no backend served the request within 10s");
        assert_eq!(served, 1);
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[1].processed_count, 1);
            assert_eq!(backends[0].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// Strict pinning: when the only pinned backend is offline, an online
    /// unpinned backend that lists the model must NOT serve it — the request
    /// fails with 503 and the pinned error message after the stuck timeout.
    #[tokio::test]
    async fn pin_all_pinned_offline_fails_503() {
        // A: a port that was just closed: the "offline" pinned backend never
        // answers a probe (its available list is empty, like after an
        // outage), so nothing about it is satisfiable.
        let offline_url = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind temp listener");
            let addr = l.local_addr().expect("temp addr");
            drop(l);
            format!("http://{}", addr)
        };
        // B: online and lists the model, but is not in the pin.
        let mock1 = spawn_mock_backend(M_V1_MODELS, None).await;

        let state = new_test_state(vec![offline_url.clone(), mock1.0.clone()]);
        {
            let mut backends = state.backends.lock().unwrap();
            openai_backend_state(&mut backends[0], &[]);
            backends[0].is_online = false; // pinned backend offline
            openai_backend_state(&mut backends[1], &["m"]);
        }
        pin_model(&state, "m", None, vec![offline_url.clone()]);

        let mut rx = enqueue_chat_request(&state, "m");
        let worker = tokio::spawn(run_worker(state.clone()));

        // stuck_timeout is 1s: the request must be dropped with 503 and the
        // pinned (user-diagnosable) error message.
        let part = tokio::time::timeout(std::time::Duration::from_secs(6), rx.recv())
            .await
            .expect("no response within 6s")
            .expect("responder closed");
        match part {
            ResponsePart::Status(status, _) => {
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE)
            }
            _ => panic!("expected Status(503), got another part"),
        }
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("no body within 2s")
            .expect("responder closed");
        match &chunk {
            ResponsePart::Chunk(bytes) => {
                let body = String::from_utf8_lossy(bytes);
                assert!(
                    body.contains(r#"no configured backend available for model 'm'"#),
                    "unexpected 503 body: {}",
                    body
                );
            }
            _ => panic!("expected Chunk with the error body, got another part"),
        }

        // The unpinned online backend must never have served the request.
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[1].processed_count, 0);
            assert_eq!(backends[1].active_requests, 0);
        }

        worker.abort();
        mock1.1.abort();
    }
}
