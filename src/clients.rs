//! Clients and their API keys (`settings.clients_file`, default
//! `clients.yaml`), managed from the web UI.
//!
//! A request carrying a client's key (`Authorization: Bearer <key>` or
//! `X-Api-Key: <key>`) queues under the client's name, whatever `X-User-ID`
//! it sends. Requests with no key or an unknown one fall back to `X-User-ID`
//! exactly as before, so a deployment without clients behaves unchanged.
//!
//! Keys are stored in plaintext so an operator can look one up again later;
//! the file should be readable by the proxy only. Every mutation is written
//! to a temporary file and renamed over the old one, and the in-memory state
//! only changes once that write succeeded.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::constant_time_eq;

pub const DEFAULT_CLIENTS_FILE: &str = "clients.yaml";

const MIN_KEY_LEN: usize = 8;
const MAX_KEY_LEN: usize = 256;
const MAX_NAME_LEN: usize = 64;
const MAX_TEXT_LEN: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey {
    /// Generated at load when a hand-written file leaves it out.
    #[serde(default)]
    pub id: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// Keep this key's log entries but store no request/response text for the
    /// requests it sends (see [`Client::redact_log`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redact_log: bool,
    #[serde(default, skip_serializing_if = "is_zero_ms")]
    pub created_at_ms: u64,
}

/// A key in a hand-written file: the bare key string, or the full form.
#[derive(Deserialize)]
#[serde(untagged)]
enum KeySpec {
    Bare(String),
    Full(ApiKey),
}

fn de_keys<'de, D>(d: D) -> Result<Vec<ApiKey>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let specs = Vec::<KeySpec>::deserialize(d)?;
    Ok(specs
        .into_iter()
        .map(|spec| match spec {
            KeySpec::Bare(key) => ApiKey {
                id: String::new(),
                key,
                label: String::new(),
                redact_log: false,
                created_at_ms: 0,
            },
            KeySpec::Full(k) => k,
        })
        .collect())
}

pub(crate) fn is_zero_ms(v: &u64) -> bool {
    *v == 0
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Client {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Scheduling priority that survives restarts: at most one client is
    /// VIP and at most one is boosted (mirrors `AppState::vip_user` /
    /// `boost_user`, which the scheduler reads).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vip: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub boost: bool,
    /// Log entries stay, but no request/response body text is stored for
    /// requests this client sends (and none for its keys' requests either — a
    /// key can only turn capture off, never re-enable it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redact_log: bool,
    #[serde(default, skip_serializing_if = "is_zero_ms")]
    pub created_at_ms: u64,
    #[serde(default, deserialize_with = "de_keys", skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<ApiKey>,
}

#[derive(Default, Serialize, Deserialize)]
struct ClientsFile {
    #[serde(default)]
    clients: Vec<Client>,
}

/// A key as the web UI lists it: the stored key plus when it last
/// identified a request (in memory only, so `0` after a restart).
#[derive(Serialize)]
pub struct KeyView {
    #[serde(flatten)]
    pub key: ApiKey,
    pub last_used_ms: u64,
}

#[derive(Serialize)]
pub struct ClientView {
    pub name: String,
    pub note: String,
    pub disabled: bool,
    pub vip: bool,
    pub boost: bool,
    pub redact_log: bool,
    pub created_at_ms: u64,
    pub keys: Vec<KeyView>,
}

/// Who a request's API key belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub client: String,
    pub disabled: bool,
    /// True when body-text capture is off for this request: the client's flag
    /// or the key's own flag (`ApiKey::redact_log`).
    pub redact_log: bool,
    /// The key that matched (id and label; for metrics, never the secret).
    pub key_id: String,
    pub key_label: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    Invalid(String),
    NotFound(String),
    Conflict(String),
    Io(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Invalid(m)
            | StoreError::NotFound(m)
            | StoreError::Conflict(m)
            | StoreError::Io(m) => f.write_str(m),
        }
    }
}

/// Changes to a client; `None` leaves a field as it is.
#[derive(Default, Deserialize)]
pub struct ClientPatch {
    pub name: Option<String>,
    pub note: Option<String>,
    pub disabled: Option<bool>,
    pub redact_log: Option<bool>,
}

pub struct ClientStore {
    /// `None` keeps the store in memory only (tests).
    path: Option<PathBuf>,
    clients: Vec<Client>,
    /// Key id → epoch ms of the last request it identified.
    last_used: HashMap<String, u64>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("OS random number generator unavailable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A fresh API key: `omq-` and 128 bits of randomness.
pub fn generate_key() -> String {
    format!("omq-{}", random_hex(16))
}

/// Client names become queue user ids, so they stay short and plain.
pub fn validate_name(name: &str) -> Result<(), StoreError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "client name must be 1–{MAX_NAME_LEN} characters of letters, digits, '.', '_' or '-'"
        )))
    }
}

/// Keys travel in an HTTP header, so: printable ASCII, no whitespace.
pub fn validate_key(key: &str) -> Result<(), StoreError> {
    let ok = (MIN_KEY_LEN..=MAX_KEY_LEN).contains(&key.len())
        && key.bytes().all(|b| b.is_ascii_graphic());
    if ok {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "key must be {MIN_KEY_LEN}–{MAX_KEY_LEN} printable ASCII characters with no whitespace"
        )))
    }
}

fn clip(text: &str) -> String {
    text.trim().chars().take(MAX_TEXT_LEN).collect()
}

impl ClientStore {
    /// An empty store that is never written to disk.
    pub fn in_memory() -> Self {
        Self {
            path: None,
            clients: Vec::new(),
            last_used: HashMap::new(),
        }
    }

    /// Load the store from `path`; a missing file is an empty store. A file
    /// that exists but cannot be parsed is an error: starting with an empty
    /// store would overwrite every key on the first change.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let mut clients = match read_store_or_migrate::<ClientsFile>(&path)? {
            Some(f) => f.clients,
            None => Vec::new(),
        };
        for c in &mut clients {
            for k in &mut c.keys {
                if k.id.is_empty() {
                    k.id = format!("k{}", random_hex(6));
                }
            }
        }
        Ok(Self {
            path: Some(path),
            clients,
            last_used: HashMap::new(),
        })
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    /// Find the client owning `key`. Every stored key is compared in constant
    /// time, so the lookup does not reveal how much of a key matched.
    pub fn resolve(&mut self, key: &str) -> Option<Resolved> {
        let mut found = None;
        for c in &self.clients {
            for k in &c.keys {
                if constant_time_eq(&k.key, key) && found.is_none() {
                    found = Some((
                        k.id.clone(),
                        k.label.clone(),
                        c.name.clone(),
                        c.disabled,
                        c.redact_log || k.redact_log,
                    ));
                }
            }
        }
        let (key_id, key_label, client, disabled, redact_log) = found?;
        self.last_used.insert(key_id.clone(), now_ms());
        Some(Resolved {
            client,
            disabled,
            redact_log,
            key_id,
            key_label,
        })
    }

    pub fn list(&self) -> Vec<ClientView> {
        self.clients
            .iter()
            .map(|c| ClientView {
                name: c.name.clone(),
                note: c.note.clone(),
                disabled: c.disabled,
                vip: c.vip,
                boost: c.boost,
                redact_log: c.redact_log,
                created_at_ms: c.created_at_ms,
                keys: c
                    .keys
                    .iter()
                    .map(|k| KeyView {
                        key: k.clone(),
                        last_used_ms: self.last_used.get(&k.id).copied().unwrap_or(0),
                    })
                    .collect(),
            })
            .collect()
    }

    /// Whether `name` is a registered client. Registered names are reserved:
    /// only a request presenting one of the client's keys may queue under it.
    pub fn is_registered(&self, name: &str) -> bool {
        self.clients.iter().any(|c| c.name == name)
    }

    /// True only for a registered client that is disabled.
    pub fn is_disabled(&self, name: &str) -> bool {
        self.clients.iter().any(|c| c.name == name && c.disabled)
    }

    pub fn names(&self) -> Vec<String> {
        self.clients.iter().map(|c| c.name.clone()).collect()
    }

    pub fn disabled_names(&self) -> Vec<String> {
        self.clients
            .iter()
            .filter(|c| c.disabled)
            .map(|c| c.name.clone())
            .collect()
    }

    pub fn set_disabled(&mut self, name: &str, disabled: bool) -> Result<(), StoreError> {
        self.update_client(
            name,
            ClientPatch {
                disabled: Some(disabled),
                ..Default::default()
            },
        )
    }

    /// The persisted (VIP, boost) clients.
    pub fn priority(&self) -> (Option<String>, Option<String>) {
        let pick =
            |f: fn(&Client) -> bool| self.clients.iter().find(|c| f(c)).map(|c| c.name.clone());
        (pick(|c| c.vip), pick(|c| c.boost))
    }

    /// Record which clients hold VIP and boost (`None`, or a name that is not
    /// a registered client, clears the flag everywhere). Writes only when
    /// something changed.
    pub fn set_priority(
        &mut self,
        vip: Option<&str>,
        boost: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut next = self.clients.clone();
        for c in &mut next {
            c.vip = vip == Some(c.name.as_str());
            c.boost = boost == Some(c.name.as_str());
        }
        if next == self.clients {
            return Ok(());
        }
        self.commit(next)
    }

    pub fn create_client(&mut self, name: &str, note: &str) -> Result<(), StoreError> {
        let name = name.trim();
        validate_name(name)?;
        if self.clients.iter().any(|c| c.name == name) {
            return Err(StoreError::Conflict(format!(
                "client '{name}' already exists"
            )));
        }
        let mut next = self.clients.clone();
        next.push(Client {
            name: name.to_string(),
            note: clip(note),
            disabled: false,
            redact_log: false,
            vip: false,
            boost: false,
            created_at_ms: now_ms(),
            keys: Vec::new(),
        });
        self.commit(next)
    }

    pub fn update_client(&mut self, name: &str, patch: ClientPatch) -> Result<(), StoreError> {
        let mut next = self.clients.clone();
        let idx = Self::client_idx(&next, name)?;
        if let Some(new_name) = patch.name.as_deref().map(str::trim)
            && new_name != name
        {
            validate_name(new_name)?;
            if next.iter().any(|c| c.name == new_name) {
                return Err(StoreError::Conflict(format!(
                    "client '{new_name}' already exists"
                )));
            }
            next[idx].name = new_name.to_string();
        }
        if let Some(note) = patch.note {
            next[idx].note = clip(&note);
        }
        if let Some(disabled) = patch.disabled {
            next[idx].disabled = disabled;
        }
        if let Some(redact_log) = patch.redact_log {
            next[idx].redact_log = redact_log;
        }
        self.commit(next)
    }

    pub fn delete_client(&mut self, name: &str) -> Result<(), StoreError> {
        let mut next = self.clients.clone();
        let idx = Self::client_idx(&next, name)?;
        next.remove(idx);
        self.commit(next)
    }

    /// Add a key to `client`: the given one, or a generated one when `key` is
    /// `None`. Keys are unique across all clients.
    pub fn add_key(
        &mut self,
        client: &str,
        key: Option<&str>,
        label: &str,
        redact_log: bool,
    ) -> Result<ApiKey, StoreError> {
        let key = match key.map(str::trim) {
            Some(k) => {
                validate_key(k)?;
                k.to_string()
            }
            None => generate_key(),
        };
        if self
            .clients
            .iter()
            .flat_map(|c| &c.keys)
            .any(|k| k.key == key)
        {
            return Err(StoreError::Conflict(
                "that key is already assigned to a client".into(),
            ));
        }
        let mut next = self.clients.clone();
        let idx = Self::client_idx(&next, client)?;
        let new_key = ApiKey {
            id: format!("k{}", random_hex(6)),
            key,
            label: clip(label),
            redact_log,
            created_at_ms: now_ms(),
        };
        next[idx].keys.push(new_key.clone());
        self.commit(next)?;
        Ok(new_key)
    }

    pub fn delete_key(&mut self, id: &str) -> Result<(), StoreError> {
        let mut next = self.clients.clone();
        Self::take_key(&mut next, id)?;
        self.commit(next)?;
        self.last_used.remove(id);
        Ok(())
    }

    /// Change a key's label and/or its capture switch; `None` leaves that field
    /// as it is. The key string and its owner never change — a key moves by
    /// deleting it and handing out a new one.
    pub fn update_key(
        &mut self,
        id: &str,
        label: Option<&str>,
        redact_log: Option<bool>,
    ) -> Result<(), StoreError> {
        let mut next = self.clients.clone();
        let mut hit = false;
        for c in &mut next {
            for k in &mut c.keys {
                if k.id == id {
                    if let Some(label) = label {
                        k.label = clip(label);
                    }
                    if let Some(redact_log) = redact_log {
                        k.redact_log = redact_log;
                    }
                    hit = true;
                }
            }
        }
        if !hit {
            return Err(StoreError::NotFound(format!("no key '{id}'")));
        }
        self.commit(next)
    }

    fn client_idx(clients: &[Client], name: &str) -> Result<usize, StoreError> {
        clients
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| StoreError::NotFound(format!("no client '{name}'")))
    }

    fn take_key(clients: &mut [Client], id: &str) -> Result<ApiKey, StoreError> {
        for c in clients.iter_mut() {
            if let Some(pos) = c.keys.iter().position(|k| k.id == id) {
                return Ok(c.keys.remove(pos));
            }
        }
        Err(StoreError::NotFound(format!("no key '{id}'")))
    }

    /// Persist `next`, then make it the live state.
    fn commit(&mut self, next: Vec<Client>) -> Result<(), StoreError> {
        if let Some(path) = &self.path {
            write_atomic(path, &next)
                .map_err(|e| StoreError::Io(format!("cannot save {}: {e}", path.display())))?;
        }
        self.clients = next;
        Ok(())
    }
}

fn write_atomic(path: &Path, clients: &[Client]) -> std::io::Result<()> {
    write_store_atomic(
        path,
        &ClientsFile {
            clients: clients.to_vec(),
        },
    )
}

/// Read a YAML store; `None` when there is no file at all. When the YAML file
/// is missing but a `.json` sibling exists (stores were JSON before this
/// format change), the JSON is parsed, written back as YAML, and renamed to
/// `.json.bak`. Shared with the model-policy store.
pub(crate) fn read_store_or_migrate<T: DeserializeOwned + Serialize>(
    path: &Path,
) -> Result<Option<T>, String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            return serde_yaml::from_str::<T>(&text)
                .map(Some)
                .map_err(|e| format!("cannot parse {}: {e}", path.display()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    }
    let is_yaml = matches!(path.extension().and_then(|e| e.to_str()), Some("yaml") | Some("yml"));
    let json = path.with_extension("json");
    if !is_yaml || !json.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&json).map_err(|e| format!("cannot read {}: {e}", json.display()))?;
    let value: T =
        serde_json::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", json.display()))?;
    write_store_atomic(path, &value).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let bak = json.with_extension("json.bak");
    fs::rename(&json, &bak).map_err(|e| format!("cannot rename {}: {e}", json.display()))?;
    tracing::info!("Migrated {} to {} (old file kept as {})", json.display(), path.display(), bak.display());
    Ok(Some(value))
}

/// Write `value` as YAML to `path`: a temporary file, synced, then renamed
/// over the old one, so a crash never leaves a half-written file. Owner-only,
/// since these stores hold credentials and policy.
pub(crate) fn write_store_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let body = serde_yaml::to_string(value).map_err(std::io::Error::other)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        // A leftover temp file is removed first so the mode applies (it is
        // only set on creation).
        let _ = fs::remove_file(&tmp);
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(name: &str, key: &str) -> ClientStore {
        let mut s = ClientStore::in_memory();
        s.create_client(name, "").unwrap();
        s.add_key(name, Some(key), "", false).unwrap();
        s
    }

    #[test]
    fn resolve_finds_owner_and_records_use() {
        let mut s = store_with("alice", "alice-key-123");
        let r = s.resolve("alice-key-123").unwrap();
        assert_eq!(r.client, "alice");
        assert!(!r.disabled);
        assert!(s.list()[0].keys[0].last_used_ms > 0);
        assert_eq!(s.resolve("alice-key-12"), None);
        assert_eq!(s.resolve("nope"), None);
    }

    #[test]
    fn disabled_client_still_resolves_as_disabled() {
        let mut s = store_with("bob", "bob-key-0001");
        s.update_client(
            "bob",
            ClientPatch {
                disabled: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(s.resolve("bob-key-0001").unwrap().disabled);
    }

    #[test]
    fn rejects_bad_names_and_keys() {
        let mut s = ClientStore::in_memory();
        assert!(matches!(
            s.create_client("", ""),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            s.create_client("has space", ""),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            s.create_client(&"x".repeat(65), ""),
            Err(StoreError::Invalid(_))
        ));
        s.create_client("ok.name_1-2", "").unwrap();
        assert!(matches!(
            s.add_key("ok.name_1-2", Some("short"), "", false),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            s.add_key("ok.name_1-2", Some("has a space"), "", false),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            s.add_key("missing", Some("long-enough-key"), "", false),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn keys_and_names_are_unique() {
        let mut s = store_with("alice", "shared-key-1");
        assert!(matches!(
            s.create_client("alice", ""),
            Err(StoreError::Conflict(_))
        ));
        s.create_client("bob", "").unwrap();
        assert!(matches!(
            s.add_key("bob", Some("shared-key-1"), "", false),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(
            s.update_client(
                "bob",
                ClientPatch {
                    name: Some("alice".into()),
                    ..Default::default()
                }
            ),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn generated_keys_are_valid_and_distinct() {
        let mut s = ClientStore::in_memory();
        s.create_client("c", "").unwrap();
        let a = s.add_key("c", None, "a", false).unwrap();
        let b = s.add_key("c", None, "b", false).unwrap();
        assert!(a.key.starts_with("omq-") && a.key.len() == 36);
        assert_ne!(a.key, b.key);
        assert_ne!(a.id, b.id);
        validate_key(&a.key).unwrap();
    }

    #[test]
    fn capture_switch_or_between_client_and_key() {
        let mut s = store_with("alice", "alice-key-123");
        let kid = s.list()[0].keys[0].key.id.clone();
        assert!(!s.resolve("alice-key-123").unwrap().redact_log);

        // A key can switch its own capture off.
        s.update_key(&kid, None, Some(true)).unwrap();
        assert!(s.resolve("alice-key-123").unwrap().redact_log);

        // The client switches it for every key it owns, and un-setting the key
        // does not re-enable it against the client.
        s.update_client(
            "alice",
            ClientPatch { redact_log: Some(true), ..Default::default() },
        )
        .unwrap();
        s.update_key(&kid, None, Some(false)).unwrap();
        assert!(s.resolve("alice-key-123").unwrap().redact_log);

        // Only the client back off: capture is on again.
        s.update_client(
            "alice",
            ClientPatch { redact_log: Some(false), ..Default::default() },
        )
        .unwrap();
        assert!(!s.resolve("alice-key-123").unwrap().redact_log);
        assert!(!serde_yaml::to_string(&s.list()[0].keys[0].key).unwrap().contains("redact_log"));
    }

    #[test]
    fn delete_key_and_keep_its_owner() {
        let mut s = store_with("alice", "moving-key-1");
        s.create_client("bob", "").unwrap();
        let id = s.list()[0].keys[0].key.id.clone();
        s.delete_key(&id).unwrap();
        assert_eq!(s.resolve("moving-key-1"), None);
        assert!(matches!(s.delete_key(&id), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn rename_keeps_keys() {
        let mut s = store_with("old", "rename-key-1");
        s.update_client(
            "old",
            ClientPatch {
                name: Some("new".into()),
                note: Some("  hello ".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.resolve("rename-key-1").unwrap().client, "new");
        assert_eq!(s.list()[0].note, "hello");
    }

    #[test]
    fn registration_disable_and_priority() {
        let mut s = store_with("alice", "alice-key-123");
        s.create_client("bob", "").unwrap();
        assert!(s.is_registered("alice") && !s.is_registered("carol"));
        assert!(!s.is_disabled("alice") && !s.is_disabled("carol"));
        s.set_disabled("alice", true).unwrap();
        assert!(s.is_disabled("alice"));
        assert_eq!(s.disabled_names(), vec!["alice".to_string()]);

        s.set_priority(Some("bob"), Some("alice")).unwrap();
        assert_eq!(s.priority(), (Some("bob".into()), Some("alice".into())));
        // An unregistered name clears the flag rather than failing.
        s.set_priority(Some("carol"), None).unwrap();
        assert_eq!(s.priority(), (None, None));
        let v = s.list();
        assert!(v.iter().all(|c| !c.vip && !c.boost));
    }

    #[test]
    fn persists_round_trip_and_refuses_corrupt_file() {
        let dir = std::env::temp_dir().join(format!("omq-clients-{}", random_hex(6)));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clients.yaml");

        let mut s = ClientStore::load(&path).unwrap();
        assert_eq!(s.len(), 0);
        s.create_client("alice", "note").unwrap();
        s.add_key("alice", Some("persisted-key"), "laptop", false).unwrap();

        let mut again = ClientStore::load(&path).unwrap();
        assert_eq!(again.resolve("persisted-key").unwrap().client, "alice");
        assert_eq!(again.list()[0].keys[0].key.label, "laptop");
        assert!(!dir.join("clients.yaml.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        fs::write(&path, "{ not yaml").unwrap();
        assert!(ClientStore::load(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A hand-written file: bare key strings, no ids or timestamps.
    #[test]
    fn hand_written_yaml_gets_usable_ids() {
        let dir = std::env::temp_dir().join(format!("omq-clients-hw-{}", random_hex(6)));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clients.yaml");
        fs::write(
            &path,
            "clients:\n  - name: dm\n    keys:\n      - omq-handwritten-key\n      - { key: omq-labeled-key, label: laptop }\n  - name: idle\n",
        )
        .unwrap();
        let mut s = ClientStore::load(&path).unwrap();
        assert_eq!(s.resolve("omq-handwritten-key").unwrap().client, "dm");
        assert_eq!(s.resolve("omq-labeled-key").unwrap().client, "dm");
        assert!(s.list()[0].keys.iter().all(|k| !k.key.id.is_empty()));
        // A change persists the store with full detail; the file still loads.
        s.add_key("idle", Some("idle-key-1234"), "", false).unwrap();
        assert!(ClientStore::load(&path).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// An old `clients.json` is converted on first load and kept as `.bak`.
    #[test]
    fn legacy_json_migrates_to_yaml() {
        let dir = std::env::temp_dir().join(format!("omq-clients-mig-{}", random_hex(6)));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("clients.json"),
            r#"{"clients":[{"name":"old","note":"","disabled":false,"created_at_ms":1,"keys":[{"id":"k1","key":"legacy-key-1","label":"","created_at_ms":1}]}]}"#,
        )
        .unwrap();
        let mut s = ClientStore::load(dir.join("clients.yaml")).unwrap();
        assert_eq!(s.resolve("legacy-key-1").unwrap().client, "old");
        assert!(dir.join("clients.yaml").exists());
        assert!(dir.join("clients.json.bak").exists());
        assert!(!dir.join("clients.json").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
