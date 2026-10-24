//! Conversation log: one line per conversation in a size-rotated JSONL file.
//!
//! The proxy used to write one line per inbound request and one per upstream
//! response, which scattered a conversation over 2·N lines and left every reader
//! to re-join them. Here a conversation — the turns of one client's session —
//! is assembled in memory and written as **one line** carrying every turn with
//! absolute unix-ms timestamps, so the file, the web UI and a `jq` pipe all see
//! the same conversation the operator had with their agent.
//!
//! Notes arrive on the request path and only touch memory (a mutex update, like
//! the rest of the bookkeeping in this crate); a background task serializes and
//! appends lines and rotates the file, so no request task ever does log I/O. A
//! conversation is written once it has been quiet for `flush_idle`, and written
//! again (as one longer line under the same key) when it resumes — so a reader
//! keeps **the line with the newest `end_ms`** for each key.
//!
//! When body-text capture is switched off for a client, key or backend, no text
//! is ever handed to the store; the turn still records its timeline, sizes,
//! status and per-message roles, so the log says what happened even when it
//! cannot say what was said.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::warn;

use crate::lock::LockExt;

/// Schema marker of a conversation line. Lines from the old per-request format
/// (`{"dir":"IN"|"OUT", …}`) carry no `v` and are ignored when reading back.
pub const SCHEMA_V: u64 = 2;

/// Default conversation-log file name: used when no path is configured, and
/// again by [`ConvLogger::open_candidates`] when the configured one is
/// unusable on this host.
pub const DEFAULT_LOG_FILE: &str = "ollamamq-requests.jsonl";

/// Conversations kept in memory at once; the quietest are dropped past this.
const MAX_CONVS: usize = 200;
/// Captured body text held in memory across all conversations.
const MAX_CONTENT_BYTES: usize = 64 << 20;
/// Turns kept per conversation; a longer conversation keeps its newest turns.
const MAX_TURNS: usize = 500;
/// How often the writer looks for conversations that have gone quiet.
const SWEEP: Duration = Duration::from_secs(60);
/// Bytes of the current file read back at startup to rehydrate the store.
const SEED_TAIL_BYTES: u64 = 8 << 20;
/// Marker `truncate_utf8` appends; a body carrying it was cut at the limit.
const TRUNCATED_MARK: &str = "...[truncated:";

/// Current unix time in milliseconds (0 when the system clock is unusable).
pub fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Truncate `data` to at most `limit` bytes on a UTF-8 boundary. If the data
/// fits, it is returned lossily; otherwise the first valid prefix is kept and a
/// truncation marker with the original total size is appended. Never panics on
/// invalid UTF-8 input.
pub fn truncate_utf8(data: &[u8], limit: usize) -> String {
    if data.len() <= limit {
        return String::from_utf8_lossy(data).into_owned();
    }
    let mut end = limit;
    while end > 0 {
        match std::str::from_utf8(&data[..end]) {
            Ok(s) => return format!("{}\n{} {} bytes total]", s, TRUNCATED_MARK, data.len()),
            Err(e) => end = e.valid_up_to(),
        }
    }
    format!("\n{} {} bytes total]", TRUNCATED_MARK, data.len())
}

// ------------------------------------------------------------------ records

/// What the log knows about one message of a request body **without** keeping
/// its text: role, size, and the tool it carries or calls. This is what survives
/// capture being switched off, and what the UI draws as a chip in place of the
/// message itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MsgMeta {
    pub role: String,
    /// Characters of text content (0 for a message with none).
    #[serde(default)]
    pub chars: usize,
    /// Tool or message name, when it carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Tool-call names issued by this message (assistant messages).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<String>,
}

/// One turn's timeline in absolute unix ms. Stages the proxy could not observe
/// (or a turn that died before reaching them) stay absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StageMs {
    /// Request body received and put in the client's queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<u64>,
    /// Handed to a backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch: Option<u64>,
    /// First generated token (streamed) or first byte of the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_token: Option<u64>,
    /// Response finished or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u64>,
}

impl StageMs {
    fn is_empty(&self) -> bool {
        self.queue.is_none() && self.dispatch.is_none() && self.first_token.is_none() && self.end.is_none()
    }
}

/// Size and capture state of one direction of a turn; recorded whether or not
/// the text itself is kept, so a redacted turn still says how big it was.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BodyMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Characters kept (absent when no text was captured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chars: Option<usize>,
    /// The capture was cut at the configured content limit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Captured text of one turn. Absent when capture is off; one half missing
/// means that direction was never captured (no response yet, or an error).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<Arc<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resp: Option<Arc<String>>,
}

impl TurnContent {
    fn bytes(&self) -> usize {
        self.req.as_deref().map_or(0, |s| s.len())
            + self.resp.as_deref().map_or(0, |s| s.len())
    }
}

/// One proxied request/response pair of a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// Log correlation id, shared by a request and its response.
    pub rid: u64,
    /// When the request was received — the turn's anchor.
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "StageMs::is_empty")]
    pub tl: StageMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// `ok`, `timeout`, `client_gone`, …; absent while the turn is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<BodyMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resp: Option<BodyMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    /// Prefill seconds lost to a cache eviction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waste_secs: Option<f64>,
    /// The request's messages as roles and sizes (never their text).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub msgs: Vec<MsgMeta>,
    /// True when text capture was switched off for this turn's client, key or
    /// backend: the entry is complete, only the text is deliberately absent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<TurnContent>,
}

impl Turn {
    fn content_bytes(&self) -> usize {
        self.content.as_ref().map_or(0, TurnContent::bytes)
    }
}

/// One conversation: the turns of one client session, oldest turn first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conv {
    pub v: u64,
    /// Store key: `client#session`, or `client·rid` when the request carries no
    /// conversation tag.
    pub key: String,
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Human label the client sent (`X-Session-Name`); display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Last backend that served the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Highest scheduling level the conversation has been served at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// True when any turn had text capture switched off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
    pub start_ms: u64,
    /// When the newest turn arrived or finished — the conversation's clock.
    pub end_ms: u64,
    /// Turns dropped from the front once the per-conversation cap was hit.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dropped_turns: usize,
    #[serde(default)]
    pub turns: Vec<Turn>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl Conv {
    fn content_bytes(&self) -> usize {
        self.turns.iter().map(Turn::content_bytes).sum()
    }

    /// The conversation with all captured text removed: what the web API sends
    /// as the turn index, so a 600 KB resent prompt is not refetched on every
    /// navigation.
    fn without_content(mut self) -> Self {
        for t in self.turns.iter_mut() {
            t.content = None;
        }
        self
    }
}

/// A conversation plus the bookkeeping the writer needs (never serialized).
struct Entry {
    conv: Conv,
    /// Unwritten changes.
    dirty: bool,
    /// Bumped on every change, so a flush that raced a resume cannot clear the
    /// dirty flag the resume set.
    rev: u64,
    /// When this conversation last changed (drives flush order and eviction).
    last_ms: u64,
}

/// One change to the store, produced by the proxy's request path.
pub enum Note {
    /// A request arrived: opens (or reopens) the conversation and its turn.
    Begin(Box<Begin>),
    /// The request's body text, already truncated to the content limit.
    ReqText {
        key: String,
        rid: u64,
        bytes: Option<u64>,
        text: Arc<String>,
    },
    /// Timeline progress on an open turn.
    Stage {
        key: String,
        rid: u64,
        queue: Option<u64>,
        dispatch: Option<u64>,
        first_token: Option<u64>,
    },
    /// The response finished: closes the turn.
    Resp(Box<Resp>),
    /// The response's text, already truncated.
    RespText {
        key: String,
        rid: u64,
        bytes: Option<u64>,
        text: Arc<String>,
    },
}

pub struct Begin {
    pub key: String,
    pub rid: u64,
    pub user: String,
    pub session_label: Option<String>,
    pub model: Option<String>,
    pub method: String,
    pub path: String,
    pub bytes: Option<u64>,
    pub priority: String,
    pub at_ms: u64,
    pub queue_ms: Option<u64>,
    pub redacted: bool,
    pub msgs: Vec<MsgMeta>,
}

pub struct Resp {
    pub key: String,
    pub rid: u64,
    pub end_ms: u64,
    /// Measured first-token time, when the response produced one.
    pub first_token: Option<u64>,
    pub status: Option<u16>,
    pub resp_bytes: Option<u64>,
    pub backend: Option<String>,
    pub outcome: String,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub waste_secs: Option<f64>,
    pub redacted: bool,
}

/// Which conversation a web request names. A conversation tag is a hash of the
/// client's own conversation id, so the tag alone identifies it; `user` narrows
/// it when the caller knows the client, and `rid` jumps straight to a request.
#[derive(Default)]
pub struct Find<'a> {
    /// The store key verbatim (`client#session`): what a viewer holds once it
    /// has a conversation, and what reload re-reads by.
    pub key: Option<&'a str>,
    pub session: Option<&'a str>,
    pub user: Option<&'a str>,
    pub rid: Option<u64>,
}

/// A conversation as the list endpoint reports it.
#[derive(Serialize)]
pub struct ConvSummary {
    pub key: String,
    pub user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    pub turns: usize,
    pub start_ms: u64,
    pub end_ms: u64,
    pub redacted: bool,
    /// True while a turn is still waiting for its response.
    pub live: bool,
    pub dropped_turns: usize,
}

/// Names of the tools a request's messages carry — the results the agent ran
/// between the previous turn and this one, and any calls it is now asking for.
/// The Overview names these on the idle line between two turns, which is what
/// the quiet stretch in a conversation actually was.
pub fn tool_names(msgs: &[MsgMeta]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in msgs {
        for n in m.calls.iter().chain(m.name.iter()) {
            if !out.iter().any(|x| x == n) {
                out.push(n.clone());
            }
        }
        if out.len() >= 3 {
            out.truncate(3);
            break;
        }
    }
    out
}

/// The store key of a request: its conversation tag when it has one, otherwise
/// the request itself, so session-less traffic becomes a one-turn conversation
/// instead of being merged with unrelated requests.
pub fn key_for(user: &str, session: Option<&str>, rid: u64) -> String {
    match session {
        Some(s) => format!("{user}#{s}"),
        None => format!("{user}\u{00b7}{rid}"),
    }
}

/// The request's messages as roles and sizes, read from its captured body.
/// Text is never returned here — this is what the log keeps when capture is off.
/// A body that is not a chat request (or was cut before its `messages`) yields
/// nothing; array content parts contribute their text length only.
pub fn msg_meta(body: &str) -> Vec<MsgMeta> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(messages) = v.get("messages").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    messages
        .iter()
        .map(|m| {
            let chars = match m.get("content") {
                Some(serde_json::Value::String(s)) => s.chars().count(),
                Some(serde_json::Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .map(str::chars)
                    .map(Iterator::count)
                    .sum(),
                _ => 0,
            };
            let name = ["name", "tool_name"]
                .iter()
                .filter_map(|k| m.get(*k).and_then(|n| n.as_str()))
                .find(|s| !s.is_empty())
                .map(str::to_string);
            let calls = m
                .get("tool_calls")
                .and_then(|c| c.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|c| {
                            c.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                                .or_else(|| c.get("name").and_then(|n| n.as_str()))
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default();
            MsgMeta {
                role: m
                    .get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("message")
                    .to_string(),
                chars,
                name,
                calls,
            }
        })
        .collect()
}

/// The in-memory conversation store, shared between the request path (writes),
/// the web API (reads) and the writer task (flush + prune).
pub struct ConvStore {
    entries: HashMap<String, Entry>,
    content_bytes: usize,
    /// Turns lost to the per-conversation cap.
    dropped: AtomicU64,
}

impl ConvStore {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            content_bytes: 0,
            dropped: AtomicU64::new(0),
        }
    }

    fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Conversation with this key, created (and counted as dirty) on first use.
    fn entry(&mut self, key: &str, user: &str) -> &mut Entry {
        let now = now_unix_millis();
        self.entries.entry(key.to_string()).or_insert_with(|| Entry {
            conv: Conv {
                v: SCHEMA_V,
                key: key.to_string(),
                user: user.to_string(),
                session: session_from_key(key),
                session_label: None,
                model: None,
                backend: None,
                priority: None,
                redacted: false,
                // The conversation's clock is set by its first turn, whose
                // timestamp is the one that was measured — not by the moment the
                // store happened to create the entry.
                start_ms: 0,
                end_ms: 0,
                dropped_turns: 0,
                turns: Vec::new(),
            },
            dirty: true,
            rev: 0,
            last_ms: now,
        })
    }

    fn touch(&mut self, key: &str) {
        let now = now_unix_millis();
        if let Some(e) = self.entries.get_mut(key) {
            e.dirty = true;
            e.rev += 1;
            e.last_ms = now;
        }
    }

    fn turn_mut(&mut self, key: &str, rid: u64) -> Option<&mut Turn> {
        self.entries
            .get_mut(key)
            .and_then(|e| e.conv.turns.iter_mut().rev().find(|t| t.rid == rid))
    }

    /// Attach captured text to a turn, keeping the store's byte budget honest.
    fn set_content(&mut self, key: &str, rid: u64, dir: Dir, bytes: Option<u64>, text: Arc<String>) {
        let Some(e) = self.entries.get_mut(key) else { return };
        let Some(t) = e.conv.turns.iter_mut().rev().find(|t| t.rid == rid) else {
            return;
        };
        let before = t.content_bytes();
        let content = t.content.get_or_insert_with(TurnContent::default);
        let chars = text.chars().count();
        let truncated = text.contains(TRUNCATED_MARK);
        match dir {
            Dir::Req => {
                content.req = Some(text);
                let meta = t.req.get_or_insert_with(BodyMeta::default);
                meta.bytes = bytes.or(meta.bytes);
                meta.chars = Some(chars);
                meta.truncated = truncated;
            }
            Dir::Resp => {
                content.resp = Some(text);
                let meta = t.resp.get_or_insert_with(BodyMeta::default);
                meta.bytes = bytes.or(meta.bytes);
                meta.chars = Some(chars);
                meta.truncated = truncated;
            }
        }
        self.content_bytes = self.content_bytes - before + t.content_bytes();
        e.dirty = true;
        e.rev += 1;
        e.last_ms = now_unix_millis();
    }

    /// Insert a conversation read back from the file (already written, so not
    /// dirty). An older or shorter view of a key is ignored: both the store and
    /// the file keep the newest, longest line per key.
    fn seed(&mut self, conv: Conv) {
        let key = conv.key.clone();
        let bytes = conv.content_bytes();
        if let Some(e) = self.entries.get_mut(&key) {
            if e.conv.end_ms > conv.end_ms || e.conv.turns.len() > conv.turns.len() {
                return;
            }
            self.content_bytes = self.content_bytes.saturating_sub(e.conv.content_bytes());
            e.conv = conv;
            self.content_bytes += bytes;
            e.dirty = false;
            e.last_ms = now_unix_millis();
            return;
        }
        self.content_bytes += bytes;
        self.entries.insert(
            key,
            Entry {
                conv,
                dirty: false,
                rev: 0,
                last_ms: now_unix_millis(),
            },
        );
    }

    /// Drop the quietest conversations until both budgets hold. A dirty
    /// conversation is only dropped once no clean one is left, so the cap costs
    /// memory rather than history whenever that is possible.
    fn prune(&mut self) {
        if self.content_bytes <= MAX_CONTENT_BYTES && self.entries.len() <= MAX_CONVS {
            return;
        }
        let mut quiet_first: Vec<(u64, bool, String)> = self
            .entries
            .iter()
            .map(|(k, e)| (e.last_ms, e.dirty, k.clone()))
            .collect();
        // Clean before dirty, quietest first.
        quiet_first.sort_by_key(|a| (a.1, a.0));
        for (_, dirty, key) in quiet_first {
            if self.content_bytes <= MAX_CONTENT_BYTES && self.entries.len() <= MAX_CONVS {
                break;
            }
            if dirty {
                warn!(
                    "conversation log: memory budget reached, dropping an unwritten conversation ({} held)",
                    self.content_bytes
                );
            }
            if let Some(e) = self.entries.remove(&key) {
                self.content_bytes = self.content_bytes.saturating_sub(e.conv.content_bytes());
            }
        }
    }

    /// Serialize the conversations quiet for at least `flush_idle`, quietest
    /// first. Cloned out of the lock so serializing a large conversation and the
    /// file write never block the request path.
    fn take_pending(&mut self, flush_idle: Duration) -> Vec<(String, String, u64)> {
        let now = now_unix_millis();
        let idle_ms = flush_idle.as_millis() as u64;
        let mut keys: Vec<(u64, String)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.dirty && now.saturating_sub(e.last_ms) >= idle_ms)
            .map(|(k, e)| (e.last_ms, k.clone()))
            .collect();
        keys.sort();
        let mut out = Vec::with_capacity(keys.len());
        for (_, key) in keys {
            let Some(e) = self.entries.get(&key) else { continue };
            match serde_json::to_string(&e.conv) {
                Ok(line) => out.push((line, key, e.rev)),
                Err(err) => warn!("conversation log: failed to serialize {key}: {err}"),
            }
        }
        out
    }

    fn find(&self, q: &Find) -> Option<&str> {
        let mut best: Option<(u64, &str)> = None;
        for (key, e) in &self.entries {
            let hit = match q {
                Find {
                    key: Some(want), ..
                } => *want == key.as_str(),
                Find {
                    rid: Some(rid),
                    session,
                    user,
                    ..
                } => {
                    e.conv.turns.iter().any(|t| t.rid == *rid)
                        && session.is_none_or(|s| e.conv.session.as_deref() == Some(s))
                        && user.is_none_or(|u| e.conv.user == u)
                }
                Find {
                    key: None,
                    session: Some(s),
                    user,
                    ..
                } => {
                    e.conv.session.as_deref() == Some(s)
                        && user.is_none_or(|u| e.conv.user == u)
                }
                Find {
                    key: None,
                    user: Some(u),
                    session: None,
                    rid: None,
                } => e.conv.user == *u,
                _ => false,
            };
            if !hit {
                continue;
            }
            if best.is_none_or(|(ms, _)| e.conv.end_ms > ms) {
                best = Some((e.conv.end_ms, key.as_str()));
            }
        }
        best.map(|(_, k)| k)
    }

    fn summaries(&self, limit: usize) -> Vec<ConvSummary> {
        let mut rows: Vec<&Conv> = self.entries.values().map(|e| &e.conv).collect();
        rows.sort_by_key(|c| std::cmp::Reverse(c.end_ms));
        rows.into_iter()
            .take(limit)
            .map(|c| ConvSummary {
                key: c.key.clone(),
                user: c.user.clone(),
                session: c.session.clone(),
                session_label: c.session_label.clone(),
                model: c.model.clone(),
                backend: c.backend.clone(),
                priority: c.priority.clone(),
                turns: c.turns.len(),
                start_ms: c.start_ms,
                end_ms: c.end_ms,
                redacted: c.redacted,
                live: c.turns.iter().any(|t| t.tl.end.is_none()),
                dropped_turns: c.dropped_turns,
            })
            .collect()
    }
}

/// Which half of a turn's captured text is meant.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Req,
    Resp,
}

/// Handle held by `AppState`: the request path pushes [`Note`]s, the web API
/// reads, and the writer task flushes. Cheap to clone (the store is shared).
#[derive(Clone)]
pub struct ConvLogger {
    store: Arc<Mutex<ConvStore>>,
    /// `None` when the log is disabled (unwritable path): notes are dropped and
    /// the store stays empty, so the API reports "no conversations".
    file: Option<Arc<FileTarget>>,
}

struct FileTarget {
    path: String,
    max_bytes: u64,
    max_files: usize,
}

impl ConvLogger {
    /// Open (or create) the log file, read its tail back into the store, and
    /// start the writer task. A missing parent directory is created: naming a
    /// log path whose directory is not there yet is a normal setup step, not a
    /// reason to run without capture. The initial open is synchronous so a bad
    /// path or permission error surfaces to the caller, which tries
    /// [`Self::open_candidates`] and then falls back to [`Self::disabled`].
    pub fn start(
        path: String,
        max_bytes: u64,
        max_files: usize,
        flush_idle: Duration,
    ) -> Result<(Self, tokio::task::JoinHandle<()>), std::io::Error> {
        if let Some(dir) = std::path::Path::new(&path).parent()
            && !dir.as_os_str().is_empty()
        {
            // A directory in the way is as fatal as an unwritable parent:
            // better to report the open failure than to log next to it.
            if dir.exists() && !dir.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    format!("{} is not a directory", dir.display()),
                ));
            }
            std::fs::create_dir_all(dir)?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let store = Arc::new(Mutex::new(ConvStore::new()));
        let target = Arc::new(FileTarget {
            path,
            max_bytes,
            max_files: max_files.max(1),
        });
        {
            let seeded = seed_from_tail(&target.path);
            if !seeded.is_empty() {
                let mut s = store.lock_or_recover();
                for conv in seeded {
                    s.seed(conv);
                }
            }
        }
        let logger = Self {
            store: Arc::clone(&store),
            file: Some(Arc::clone(&target)),
        };
        let handle = tokio::spawn(writer_loop(target, store, flush_idle));
        Ok((logger, handle))
    }

    /// No-op logger for a log that could not be opened.
    pub fn disabled() -> Self {
        Self {
            store: Arc::new(Mutex::new(ConvStore::new())),
            file: None,
        }
    }

    /// Paths worth trying, in order, when the configured one cannot be opened.
    /// First the configured path itself (its directories are created by
    /// [`Self::start`], so a plain missing directory already succeeded there),
    /// then `<fallback_dir>/ollamamq-requests.jsonl` — the store directory next
    /// to `appconf.yaml`, where every other state file lives. Without the
    /// second try a host whose config still names a container path (`/data/…`)
    /// runs with capture silently off, so nothing can be looked up later.
    /// Duplicates (the default path already next to the config) are dropped.
    pub fn open_candidates(configured: &str, fallback_dir: &Path) -> Vec<String> {
        let mut out = vec![configured.to_string()];
        let alt = fallback_dir.join(DEFAULT_LOG_FILE);
        match alt.to_str() {
            Some(s) if !out.iter().any(|c| c == s) => out.push(s.to_string()),
            _ => {}
        }
        out
    }

    /// `true` when a log file was opened at boot, i.e. bodies are being
    /// captured. The UI says this out loud instead of letting every row fail to
    /// open with the same error as an evicted conversation.
    pub fn enabled(&self) -> bool {
        self.file.is_some()
    }

    /// Where conversations are written, when capturing.
    pub fn path(&self) -> Option<&str> {
        self.file.as_ref().map(|f| f.path.as_str())
    }

    /// Turns lost to the per-conversation cap (reported where the old logger's
    /// dropped-record counter was).
    pub fn dropped_count(&self) -> u64 {
        self.store.lock_or_recover().dropped_count()
    }

    /// Apply one change. The work under the lock is a handful of moves and a
    /// size update — no parsing and no I/O — so this is safe on the request path.
    pub fn note(&self, note: Note) {
        if self.file.is_none() {
            return;
        }
        let mut store = self.store.lock_or_recover();
        match note {
            Note::Begin(b) => {
                // Keep the newest turns past the cap. A reader distinguishes
                // this line from an older one by `end_ms`, which only grows.
                if store.entries.get(&b.key).is_some_and(|e| e.conv.turns.len() >= MAX_TURNS)
                {
                    if let Some(e) = store.entries.get_mut(&b.key) {
                        e.conv.turns.remove(0);
                        e.conv.dropped_turns += 1;
                    }
                    store.dropped.fetch_add(1, Ordering::Relaxed);
                }
                let e = store.entry(&b.key, &b.user);
                if e.conv.session_label.is_none() {
                    e.conv.session_label = b.session_label;
                }
                if b.model.is_some() {
                    e.conv.model = b.model.clone();
                }
                if priority_above(e.conv.priority.as_deref(), &b.priority) {
                    e.conv.priority = Some(b.priority.clone());
                }
                if e.conv.turns.is_empty() {
                    e.conv.start_ms = b.at_ms;
                }
                e.conv.end_ms = e.conv.end_ms.max(b.at_ms);
                e.conv.redacted |= b.redacted;
                e.conv.turns.push(Turn {
                    rid: b.rid,
                    at_ms: b.at_ms,
                    tl: StageMs {
                        queue: b.queue_ms,
                        dispatch: None,
                        first_token: None,
                        end: None,
                    },
                    method: Some(b.method),
                    path: b.path,
                    model: b.model,
                    backend: None,
                    status: None,
                    outcome: None,
                    priority: Some(b.priority),
                    req: Some(BodyMeta {
                        bytes: b.bytes,
                        chars: None,
                        truncated: false,
                    }),
                    resp: None,
                    prompt_tokens: None,
                    completion_tokens: None,
                    waste_secs: None,
                    msgs: b.msgs,
                    redacted: b.redacted,
                    content: None,
                });
                e.dirty = true;
                e.rev += 1;
                e.last_ms = now_unix_millis();
            }
            Note::ReqText {
                key,
                rid,
                bytes,
                text,
            } => store.set_content(&key, rid, Dir::Req, bytes, text),
            Note::RespText {
                key,
                rid,
                bytes,
                text,
            } => store.set_content(&key, rid, Dir::Resp, bytes, text),
            Note::Stage {
                key,
                rid,
                queue,
                dispatch,
                first_token,
            } => {
                if let Some(t) = store.turn_mut(&key, rid) {
                    t.tl.queue = queue.or(t.tl.queue);
                    t.tl.dispatch = dispatch.or(t.tl.dispatch);
                    t.tl.first_token = first_token.or(t.tl.first_token);
                }
                store.touch(&key);
            }
            Note::Resp(r) => {
                let key = r.key.clone();
                if let Some(t) = store.turn_mut(&key, r.rid) {
                    t.tl.end = Some(r.end_ms);
                    t.tl.first_token = r.first_token.or(t.tl.first_token);
                    let meta = t.resp.get_or_insert_with(BodyMeta::default);
                    meta.bytes = r.resp_bytes.or(meta.bytes);
                    t.status = r.status.or(t.status);
                    t.outcome = Some(r.outcome.clone());
                    t.backend = r.backend.clone().or_else(|| t.backend.clone());
                    t.prompt_tokens = r.prompt_tokens.or(t.prompt_tokens);
                    t.completion_tokens = r.completion_tokens.or(t.completion_tokens);
                    t.waste_secs = r.waste_secs.or(t.waste_secs);
                    t.redacted |= r.redacted;
                }
                if let Some(e) = store.entries.get_mut(&key) {
                    if r.backend.is_some() {
                        e.conv.backend = r.backend.clone();
                    }
                    e.conv.end_ms = e.conv.end_ms.max(r.end_ms);
                    e.conv.redacted |= r.redacted;
                    e.dirty = true;
                    e.rev += 1;
                    e.last_ms = now_unix_millis();
                }
            }
        }
        store.prune();
    }

    // --- reads (web API) ---

    /// Conversations newest-first, without turns.
    pub fn summaries(&self, limit: usize) -> Vec<ConvSummary> {
        self.store.lock_or_recover().summaries(limit)
    }

    /// The conversation named by `q`, with captured text stripped (fetch one
    /// turn's text with [`Self::content`]).
    pub fn index(&self, q: &Find) -> Option<Conv> {
        let key = self.store.lock_or_recover().find(q)?.to_string();
        let conv = self
            .store
            .lock_or_recover()
            .entries
            .get(&key)
            .map(|e| e.conv.clone())?;
        Some(conv.without_content())
    }

    /// Captured text of one turn (`rid` defaults to the newest turn).
    pub fn content(&self, q: &Find, dir: Dir) -> Option<String> {
        let store = self.store.lock_or_recover();
        let key = store.find(q)?;
        let e = store.entries.get(key)?;
        let turn = match q.rid {
            // `rid` already selected the conversation; use it for the turn too.
            Some(rid) => e.conv.turns.iter().rev().find(|t| t.rid == rid)?,
            None => e.conv.turns.last()?,
        };
        let c = turn.content.as_ref()?;
        match dir {
            Dir::Req => c.req.as_deref().map(|s| s.to_string()),
            Dir::Resp => c.resp.as_deref().map(|s| s.to_string()),
        }
    }
}

/// True when `candidate` outranks the conversation's recorded priority.
fn priority_above(current: Option<&str>, candidate: &str) -> bool {
    let rank = |p: &str| match p {
        "top" => 2,
        "high" => 1,
        _ => 0,
    };
    current.is_none_or(|c| rank(candidate) > rank(c))
}

/// The conversation tag part of a key (`None` for a session-less one).
fn session_from_key(key: &str) -> Option<String> {
    key.split_once('#').map(|(_, s)| s.to_string())
}

/// Read the tail of `path` and return the newest view of every conversation it
/// holds. Only `v: 2` lines are understood — the older per-request lines are
/// skipped — and the torn line a seeked tail starts with is ignored.
fn seed_from_tail(path: &str) -> Vec<Conv> {
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return Vec::new(),
    };
    let data = std::fs::File::open(path).and_then(|mut f| {
        use std::io::{Read, Seek, SeekFrom};
        if len > SEED_TAIL_BYTES {
            f.seek(SeekFrom::End(-(SEED_TAIL_BYTES as i64)))?;
        }
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    });
    let Ok(data) = data else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&data);
    let seeked = len > SEED_TAIL_BYTES;
    let mut newest: HashMap<String, Conv> = HashMap::new();
    for (i, line) in text.lines().enumerate() {
        if seeked && i == 0 {
            continue;
        }
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("v").and_then(|v| v.as_u64()) != Some(SCHEMA_V) {
            continue;
        }
        let Ok(conv) = serde_json::from_value::<Conv>(v) else {
            continue;
        };
        match newest.get(&conv.key) {
            Some(prev) if prev.end_ms >= conv.end_ms || prev.turns.len() > conv.turns.len() => {}
            _ => {
                newest.insert(conv.key.clone(), conv);
            }
        }
    }
    newest.into_values().collect()
}

/// Writer task: every `SWEEP`, append the conversations that have gone quiet.
async fn writer_loop(file: Arc<FileTarget>, store: Arc<Mutex<ConvStore>>, flush_idle: Duration) {
    // Continue sizing from the existing file so restarts keep rotation honest.
    let mut current_size = tokio::fs::metadata(&file.path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    // One handle, kept open across sweeps: a flush is one line per changed
    // conversation, and opening and closing the file for each of them was pure
    // overhead. It is dropped before a rotation and reopened afterwards.
    let mut handle: Option<tokio::fs::File> = open_append(&file.path).await.ok();
    if handle.is_none() {
        warn!(
            "conversation log: cannot open {} for writing yet; will retry on the next flush",
            file.path
        );
    }
    // Wake at least as often as the flush target, so a short
    // `request_log_flush_secs` is honoured instead of rounded up to the sweep.
    let mut ticker = tokio::time::interval(SWEEP.min(flush_idle.max(Duration::from_millis(10))));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let pending = take_pending(&store, flush_idle);
        for (line, key, rev) in pending {
            if current_size + line.len() as u64 > file.max_bytes {
                // Nothing may hold the old file while it is renamed, or the
                // writer would keep appending to the rotated copy.
                drop(handle.take());
                match rotate(&file.path, file.max_files).await {
                    Ok(()) => current_size = 0,
                    Err(e) => {
                        // Resetting the size to 0 here would disable rotation
                        // for good: every later check would start from zero and
                        // the file would grow past its ceiling in silence.
                        current_size = tokio::fs::metadata(&file.path)
                            .await
                            .map(|m| m.len())
                            .unwrap_or(current_size);
                        warn!(
                            "conversation log: rotation of {} failed ({e}); appending to it and \
                             keeping its real size ({} B) so the {} B ceiling still bites",
                            file.path, current_size, file.max_bytes
                        );
                    }
                }
            }
            match append(&mut handle, &file.path, &line).await {
                Ok(n) => current_size += n,
                Err(e) => {
                    warn!("conversation log: failed to append to {}: {}", file.path, e);
                    continue;
                }
            }
            // Clear the flag only if nothing changed while we were writing.
            let mut s = store.lock_or_recover();
            if let Some(e) = s.entries.get_mut(&key)
                && e.rev == rev
            {
                e.dirty = false;
            }
        }
    }
}

fn take_pending(store: &Arc<Mutex<ConvStore>>, flush_idle: Duration) -> Vec<(String, String, u64)> {
    store.lock_or_recover().take_pending(flush_idle)
}

async fn open_append(path: &str) -> Result<tokio::fs::File, std::io::Error> {
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
}

/// Append one flushed line through the shared handle (reopened when it is not
/// open), so a tailing reader never sees half a conversation. A failed write
/// drops the handle, so the next call starts from a fresh one. Returns the bytes
/// written.
async fn append(
    handle: &mut Option<tokio::fs::File>,
    path: &str,
    line: &str,
) -> Result<u64, std::io::Error> {
    if handle.is_none() {
        *handle = Some(open_append(path).await?);
    }
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    let f = handle
        .as_mut()
        .expect("the handle is open on this path through the function");
    match f.write_all(buf.as_bytes()).await {
        Ok(()) => match f.flush().await {
            Ok(()) => Ok(buf.len() as u64),
            Err(e) => {
                *handle = None;
                Err(e)
            }
        },
        Err(e) => {
            *handle = None;
            Err(e)
        }
    }
}

/// Shift the rotation chain: `{path}.{max_files-1}` is deleted, each existing
/// `{path}.{i}` moves up one slot (missing files skipped), and the current file
/// becomes `{path}.0`. Reports the first failure instead of swallowing it: the
/// caller has to keep the file's true size when the chain did not move.
async fn rotate(path: &str, max_files: usize) -> Result<(), String> {
    // Nothing on disk to move (deleted out of band, or a first write that has
    // not happened yet): the caller's next append recreates the file, so this
    // is not a failure.
    if tokio::fs::metadata(path).await.is_err() {
        return Ok(());
    }
    let mut failed: Option<String> = None;
    let note = |what: &str, e: std::io::Error, failed: &mut Option<String>| {
        if failed.is_none() {
            *failed = Some(format!("{what}: {e}"));
        }
    };
    let oldest = format!("{}.{}", path, max_files - 1);
    if let Err(e) = tokio::fs::remove_file(&oldest).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        note("cannot drop the oldest file", e, &mut failed);
    }
    for i in (0..max_files.saturating_sub(1)).rev() {
        let from = format!("{}.{}", path, i);
        if tokio::fs::metadata(&from).await.is_ok() {
            let to = format!("{}.{}", path, i + 1);
            if let Err(e) = tokio::fs::rename(&from, &to).await {
                note("cannot shift", e, &mut failed);
            }
        }
    }
    if let Err(e) = tokio::fs::rename(path, format!("{}.0", path)).await {
        note("cannot rotate the current file", e, &mut failed);
    }
    match failed {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("ollamamq_convlog_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        for i in 0..4 {
            let _ = std::fs::remove_file(format!("{}.{}", path.display(), i));
        }
        path.to_string_lossy().into_owned()
    }

    fn begin(key: &str, rid: u64, at: u64) -> Note {
        Note::Begin(Box::new(Begin {
            key: key.into(),
            rid,
            user: key.split('#').next().unwrap().into(),
            session_label: Some("refactor-auth".into()),
            model: Some("qwen".into()),
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            bytes: Some(100),
            priority: "normal".into(),
            at_ms: at,
            queue_ms: Some(at + 1),
            redacted: false,
            msgs: vec![MsgMeta {
                role: "user".into(),
                chars: 12,
                name: None,
                calls: vec![],
            }],
        }))
    }

    fn resp(key: &str, rid: u64, end: u64) -> Note {
        Note::Resp(Box::new(Resp {
            key: key.into(),
            rid,
            end_ms: end,
            first_token: None,
            status: Some(200),
            resp_bytes: Some(50),
            backend: Some("gpu:11434".into()),
            outcome: "ok".into(),
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            waste_secs: None,
            redacted: false,
        }))
    }

    fn read_lines(path: &str) -> Vec<Conv> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Conv>(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn turn_cap_keeps_the_newest_and_counts_the_rest() {
        let path = tmp("cap.jsonl");
        let (log, handle) =
            ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_secs(3600)).unwrap();
        for i in 0..(MAX_TURNS as u64) + 5 {
            log.note(begin("cap#s", i, 1000 + i));
        }
        let guard = log.store.lock_or_recover();
        let conv = &guard.entries["cap#s"].conv;
        assert_eq!(conv.turns.len(), MAX_TURNS);
        assert_eq!(conv.dropped_turns, 5);
        // The newest turn is present, the first five are gone.
        assert_eq!(conv.turns.last().unwrap().rid, (MAX_TURNS as u64) + 4);
        assert_eq!(conv.turns.first().unwrap().rid, 5);
        drop(guard);
        assert_eq!(log.dropped_count(), 5);
        handle.abort();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn one_line_per_conversation_after_idle() {
        let path = tmp("one_line.jsonl");
        // The writer task would flush on its own after `flush_idle`; an hour
        // keeps the file for the test alone, which drives `take_pending`.
        let (log, handle) =
            ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_secs(3600)).unwrap();
        log.note(begin("alice#a1", 1, 1000));
        log.note(Note::ReqText {
            key: "alice#a1".into(),
            rid: 1,
            bytes: Some(100),
            text: Arc::new("{\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}".into()),
        });
        log.note(resp("alice#a1", 1, 2000));
        log.note(begin("alice#a1", 2, 3000));
        log.note(resp("alice#a1", 2, 4000));
        // Two turns of one conversation, flushed as one line.
        let pending = take_pending(&log.store, Duration::ZERO);
        assert_eq!(pending.len(), 1, "one conversation must produce one line");
        let conv: Conv = serde_json::from_str(&pending[0].0).unwrap();
        assert_eq!(conv.turns.len(), 2);
        assert_eq!(conv.v, SCHEMA_V);
        assert_eq!(conv.session.as_deref(), Some("a1"));
        assert_eq!(conv.session_label.as_deref(), Some("refactor-auth"));
        assert_eq!(conv.turns[0].tl.queue, Some(1001));
        assert_eq!(conv.turns[0].tl.end, Some(2000));
        assert_eq!(conv.turns[0].content.as_ref().unwrap().req.as_deref().unwrap(), "{\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}");
        assert_eq!(conv.turns[0].req.as_ref().unwrap().chars, Some(45));
        assert_eq!(conv.start_ms, 1000);
        assert_eq!(conv.end_ms, 4000);
        assert!(!conv.redacted);

        // A flush clears the dirty flag; nothing new is pending.
        {
            let mut s = log.store.lock_or_recover();
            for (_, key, _) in &pending {
                s.entries.get_mut(key).unwrap().dirty = false;
            }
        }
        assert!(take_pending(&log.store, Duration::ZERO).is_empty());
        handle.abort();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn redacted_conversation_keeps_structure_without_text() {
        let path = tmp("redacted.jsonl");
        let (log, handle) =
            ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_secs(3600)).unwrap();
        let mut b = begin("bob#b1", 7, 5000);
        if let Note::Begin(bx) = &mut b {
            bx.redacted = true;
        }
        log.note(b);
        log.note(resp("bob#b1", 7, 6000));
        let line = take_pending(&log.store, Duration::ZERO).remove(0).0;
        let conv: Conv = serde_json::from_str(&line).unwrap();
        assert!(conv.redacted && conv.turns[0].redacted);
        // No text anywhere in the line, but the timeline and sizes are intact.
        assert!(!line.contains("\"content\""));
        assert!(!line.contains("hi"));
        let t = &conv.turns[0];
        assert_eq!(t.tl.end, Some(6000));
        assert_eq!(t.status, Some(200));
        assert_eq!(t.req.as_ref().unwrap().bytes, Some(100));
        assert_eq!(t.msgs[0].role, "user");
        assert_eq!(t.msgs[0].chars, 12);
        assert_eq!(log.content(&Find { session: Some("b1"), ..Default::default() }, Dir::Req), None);
        handle.abort();
        let _ = std::fs::remove_file(&path);
    }

    /// A rotation that cannot move the chain has to say so: the caller keeps
    /// the file's real size only when it learns the chain did not move, and a
    /// silent success would disable rotation for the rest of the process.
    #[cfg(unix)]
    #[tokio::test]
    async fn rotate_reports_a_chain_it_cannot_move() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ollamamq_rotate_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("log.jsonl");
        std::fs::write(&path, "{\"v\":2}\n").expect("write log");
        // No rename may happen in the log's directory (a read-only mount, the
        // case this exists for).
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        let outcome = rotate(path.to_str().expect("utf-8 path"), 2).await;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
        let err = outcome.expect_err("renaming into a read-only directory must fail");
        assert!(!err.is_empty(), "the failure names what went wrong");
        assert!(
            std::fs::metadata(&path).is_ok(),
            "a failed rotation leaves the current file alone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rotating_a_log_that_is_not_there_is_not_a_failure() {
        let path = tmp("never-written.jsonl");
        let _ = std::fs::remove_file(&path);
        rotate(&path, 3).await.expect("nothing to move");
    }

    #[tokio::test]
    async fn resume_rewrites_the_newest_line_for_a_key() {
        let path = tmp("resume.jsonl");
        {
            let (log, handle) =
            ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_secs(3600)).unwrap();
            log.note(begin("carol#c1", 1, 1000));
            log.note(resp("carol#c1", 1, 2000));
            let (line, key, rev) = take_pending(&log.store, Duration::ZERO).remove(0);
            append(&mut None, &path, &line).await.unwrap();
            log.store.lock_or_recover().entries.get_mut(&key).unwrap().dirty = false;
            let _ = rev;
            // The conversation resumes: one longer line, same key.
            log.note(begin("carol#c1", 2, 9000));
            log.note(resp("carol#c1", 2, 9500));
            let (line2, _, _) = take_pending(&log.store, Duration::ZERO).remove(0);
            append(&mut None, &path, &line2).await.unwrap();
            handle.abort();
        }
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        // The reader's rule: newest end_ms wins, and it holds both turns.
        let newest = lines.into_iter().max_by_key(|c| c.end_ms).unwrap();
        assert_eq!(newest.turns.len(), 2);
        assert_eq!(newest.end_ms, 9500);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn seeding_reads_back_the_newest_line_and_skips_old_format() {
        let path = tmp("seed.jsonl");
        // A v1 line (old per-request format) must be ignored.
        std::fs::write(
            &path,
            "{\"ts\":1,\"dir\":\"IN\",\"user\":\"x\",\"method\":\"GET\",\"path\":\"/\",\"priority\":\"normal\"}\n",
        )
        .unwrap();
        {
            let (log, handle) =
            ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_secs(3600)).unwrap();
            log.note(begin("dave#d1", 1, 1000));
            log.note(resp("dave#d1", 1, 2000));
            let (line, _, _) = take_pending(&log.store, Duration::ZERO).remove(0);
            append(&mut None, &path, &line).await.unwrap();
            // Shorter/older view of the same key must not overwrite the newer one.
            let (log2, h2) =
                ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_secs(3600)).unwrap();
            let seeded = log2.index(&Find {
                session: Some("d1"),
                ..Default::default()
            });
            assert!(seeded.is_some(), "the v2 line must be read back");
            assert_eq!(seeded.unwrap().turns.len(), 1);
            h2.abort();
            handle.abort();
        }
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn rotation_keeps_the_file_within_its_budget() {
        let path = tmp("rotate.jsonl");
        let body = Arc::new("x".repeat(500));
        // The writer flushes conversations that have been quiet for
        // `flush_idle`, so a short one drives the real task end to end.
        let (log, handle) =
            ConvLogger::start(path.clone(), 1200, 2, Duration::from_millis(30)).unwrap();
        for i in 0..12u64 {
            let key = format!("u#s{i}");
            log.note(begin(&key, i, 1000 + i));
            log.note(Note::ReqText {
                key: key.clone(),
                rid: i,
                bytes: Some(500),
                text: Arc::clone(&body),
            });
            log.note(resp(&key, i, 1500 + i));
        }
        // Which conversation is written last depends on the store order, and
        // rotation deletes what falls off the end — so what proves the writer
        // works is: lines arrived, they rotated, and every surviving line is a
        // whole record inside the budget.
        let mut files = Vec::new();
        for _ in 0..200 {
            let mut all = Vec::new();
            for f in [&path, &format!("{path}.0"), &format!("{path}.1")] {
                all.extend(read_lines(f));
            }
            if !all.is_empty() && std::fs::metadata(format!("{path}.0")).is_ok() {
                files = all;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        handle.abort();
        assert!(!files.is_empty(), "no conversation reached the file");
        for c in &files {
            assert_eq!(c.v, SCHEMA_V);
            assert_eq!(c.turns.len(), 1);
        }
        let cur = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert!(cur <= 1200, "current file {cur}B exceeds the budget");
        for i in 0..3 {
            let _ = std::fs::remove_file(format!("{path}.{i}"));
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn find_prefers_the_newest_conversation_and_honours_rid() {
        let mut store = ConvStore::new();
        for (key, end) in [("a#s", 1000u64), ("b#s", 2000)] {
            store.entry(key, key.split('#').next().unwrap()).conv.end_ms = end;
            store.entries.get_mut(key).unwrap().conv.turns.push(Turn {
                rid: end,
                at_ms: end,
                tl: StageMs::default(),
                method: None,
                path: "/x".into(),
                model: None,
                backend: None,
                status: None,
                outcome: None,
                priority: None,
                req: None,
                resp: None,
                prompt_tokens: None,
                completion_tokens: None,
                waste_secs: None,
                msgs: vec![],
                redacted: false,
                content: None,
            });
        }
        assert_eq!(store.find(&Find { session: Some("s"), ..Default::default() }), Some("b#s"));
        // A viewer that already holds a conversation re-reads it by key — the key
        // is the one identifier that survives a restart and a rid reuse.
        assert_eq!(store.find(&Find { key: Some("b#s"), ..Default::default() }), Some("b#s"));
        assert_eq!(store.find(&Find { key: Some("a#s"), ..Default::default() }), Some("a#s"));
        assert_eq!(store.find(&Find { key: Some("nope#s"), ..Default::default() }), None);
        assert_eq!(
            store.find(&Find { session: Some("s"), user: Some("a"), ..Default::default() }),
            Some("a#s")
        );
        assert_eq!(store.find(&Find { rid: Some(1000), ..Default::default() }), Some("a#s"));
        assert_eq!(store.find(&Find { rid: Some(99), ..Default::default() }), None);
        assert_eq!(store.find(&Find::default()), None);
    }

    #[test]
    fn msg_meta_reads_roles_sizes_and_tool_names() {
        let body = r#"{"messages":[
            {"role":"system","content":"be brief"},
            {"role":"user","content":[{"type":"text","text":"hello"}]},
            {"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":"{}"}}]},
            {"role":"tool","tool_name":"read_file","content":"file body"}
        ]}"#;
        let m = msg_meta(body);
        assert_eq!(m.len(), 4);
        assert_eq!((m[0].role.as_str(), m[0].chars), ("system", 8));
        assert_eq!((m[1].role.as_str(), m[1].chars), ("user", 5));
        assert_eq!(m[2].calls, vec!["read_file".to_string()]);
        assert_eq!(m[3].name.as_deref(), Some("read_file"));
        // Not a chat body, and a truncated one: no messages, no panic.
        assert!(msg_meta("{\"model\":\"x\"}").is_empty());
        assert!(msg_meta("{\"messages\":[{\"role\":\"u\"").is_empty());
    }

    #[test]
    fn priority_only_moves_up() {
        assert!(priority_above(None, "normal"));
        assert!(priority_above(Some("normal"), "high"));
        assert!(!priority_above(Some("high"), "normal"));
        assert!(!priority_above(Some("top"), "high"));
    }

    #[test]
    fn key_uses_the_session_tag_or_the_request() {
        assert_eq!(key_for("alice", Some("a1"), 7), "alice#a1");
        assert_eq!(key_for("alice", None, 7), "alice\u{00b7}7");
        assert_eq!(session_from_key("alice#a1").as_deref(), Some("a1"));
        assert_eq!(session_from_key("alice\u{00b7}7"), None);
    }

    #[test]
    fn truncate_utf8_boundaries() {
        assert_eq!(truncate_utf8(b"hello", 10), "hello");
        assert_eq!(truncate_utf8(b"hello", 5), "hello");
        let s = truncate_utf8(b"hello world", 5);
        assert_eq!(s, "hello\n...[truncated: 11 bytes total]");
        let data = "héllo".as_bytes();
        let s = truncate_utf8(data, 2);
        assert!(std::str::from_utf8(s.as_bytes()).is_ok());
        let data = "aé中x".as_bytes();
        for limit in 0..=data.len() + 2 {
            assert!(std::str::from_utf8(truncate_utf8(data, limit).as_bytes()).is_ok());
        }
    }

    /// A log path whose directory does not exist yet is a setup step, not a
    /// reason to run without capture — the earlier failure mode left every log
    /// row unopenable while looking like a broken viewer.
    #[tokio::test]
    async fn start_creates_the_log_directory_when_it_is_missing() {
        let dir = std::env::temp_dir().join(format!("omq-convdir-{}", crate::clients::random_hex(6)));
        let path = dir.join("nested").join("conv.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        let (log, _h) = ConvLogger::start(
            path.to_string_lossy().into_owned(),
            1 << 20,
            1,
            Duration::from_secs(3600),
        )
        .expect("a missing directory is created");
        assert!(log.enabled());
        assert!(path.exists(), "the file is opened under the new directory");
        assert_eq!(log.path(), Some(path.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A config that names a container path on a host without one still gets a
    /// second try, in the store directory next to appconf.yaml.
    #[test]
    fn open_candidates_fall_back_to_the_store_dir() {
        let store = Path::new("/etc/ollamamq");
        assert_eq!(
            ConvLogger::open_candidates("/data/ollamamq-requests.jsonl", store),
            vec![
                "/data/ollamamq-requests.jsonl".to_string(),
                "/etc/ollamamq/ollamamq-requests.jsonl".to_string(),
            ]
        );
        // Already next to the config: no duplicate second try.
        assert_eq!(
            ConvLogger::open_candidates("/etc/ollamamq/ollamamq-requests.jsonl", store),
            vec!["/etc/ollamamq/ollamamq-requests.jsonl".to_string()]
        );
    }

    #[test]
    fn disabled_logger_notes_nothing() {
        let log = ConvLogger::disabled();
        log.note(begin("x#y", 1, 1));
        assert!(log.summaries(10).is_empty());
        assert_eq!(log.dropped_count(), 0);
    }

    #[tokio::test]
    async fn start_fails_on_unwritable_path() {
        assert!(ConvLogger::start(
            "/nonexistent-dir-ollamaMQ/conv.jsonl".into(),
            1024,
            2,
            Duration::from_secs(1)
        )
        .is_err());
    }
}
