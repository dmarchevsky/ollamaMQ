//! Clients and their API keys (`settings.clients_file`, default
//! `clients.json`), managed from the web UI.
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

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::constant_time_eq;

pub const DEFAULT_CLIENTS_FILE: &str = "clients.json";

const MIN_KEY_LEN: usize = 8;
const MAX_KEY_LEN: usize = 256;
const MAX_NAME_LEN: usize = 64;
const MAX_TEXT_LEN: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub key: String,
    #[serde(default)]
    pub label: String,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Client {
    pub name: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub disabled: bool,
    pub created_at_ms: u64,
    #[serde(default)]
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
    pub created_at_ms: u64,
    pub keys: Vec<KeyView>,
}

/// Who a request's API key belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub client: String,
    pub disabled: bool,
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

fn random_hex(bytes: usize) -> String {
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
        let clients = match fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str::<ClientsFile>(&text)
                    .map_err(|e| format!("cannot parse {}: {e}", path.display()))?
                    .clients
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
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
                    found = Some((k.id.clone(), c.name.clone(), c.disabled));
                }
            }
        }
        let (id, client, disabled) = found?;
        self.last_used.insert(id, now_ms());
        Some(Resolved { client, disabled })
    }

    pub fn list(&self) -> Vec<ClientView> {
        self.clients
            .iter()
            .map(|c| ClientView {
                name: c.name.clone(),
                note: c.note.clone(),
                disabled: c.disabled,
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

    /// Move a key to another client; it keeps its id, value and label.
    pub fn move_key(&mut self, id: &str, to_client: &str) -> Result<(), StoreError> {
        let mut next = self.clients.clone();
        let to = Self::client_idx(&next, to_client)?;
        let key = Self::take_key(&mut next, id)?;
        next[to].keys.push(key);
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
    let body = serde_json::to_string_pretty(&ClientsFile {
        clients: clients.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        // The file holds plaintext keys: owner-only. A leftover temp file is
        // removed first so the mode applies (it is only set on creation).
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
        s.add_key(name, Some(key), "").unwrap();
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
            s.add_key("ok.name_1-2", Some("short"), ""),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            s.add_key("ok.name_1-2", Some("has a space"), ""),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            s.add_key("missing", Some("long-enough-key"), ""),
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
            s.add_key("bob", Some("shared-key-1"), ""),
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
        let a = s.add_key("c", None, "a").unwrap();
        let b = s.add_key("c", None, "b").unwrap();
        assert!(a.key.starts_with("omq-") && a.key.len() == 36);
        assert_ne!(a.key, b.key);
        assert_ne!(a.id, b.id);
        validate_key(&a.key).unwrap();
    }

    #[test]
    fn move_and_delete_key() {
        let mut s = store_with("alice", "moving-key-1");
        s.create_client("bob", "").unwrap();
        let id = s.list()[0].keys[0].key.id.clone();
        s.move_key(&id, "bob").unwrap();
        assert_eq!(s.resolve("moving-key-1").unwrap().client, "bob");
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
    fn persists_round_trip_and_refuses_corrupt_file() {
        let dir = std::env::temp_dir().join(format!("omq-clients-{}", random_hex(6)));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clients.json");

        let mut s = ClientStore::load(&path).unwrap();
        assert_eq!(s.len(), 0);
        s.create_client("alice", "note").unwrap();
        s.add_key("alice", Some("persisted-key"), "laptop").unwrap();

        let mut again = ClientStore::load(&path).unwrap();
        assert_eq!(again.resolve("persisted-key").unwrap().client, "alice");
        assert_eq!(again.list()[0].keys[0].key.label, "laptop");
        assert!(!dir.join("clients.json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        fs::write(&path, "{ not json").unwrap();
        assert!(ClientStore::load(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
