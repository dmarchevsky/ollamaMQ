//! Conversation log: the turns of each client session in a size-rotated JSONL
//! chain.
//!
//! A conversation — the turns of one client's session — is assembled in memory
//! with absolute unix-ms timestamps. Notes arrive on the request path and only
//! touch memory (a mutex update, like the rest of the bookkeeping in this
//! crate); a background task encodes and appends lines and rotates the file, so
//! no request task ever does log I/O.
//!
//! **On disk (schema v4) a line carries only what changed.** Once a
//! conversation has been quiet for `flush_idle`, one line is appended with its
//! header and the turns that changed since they were last written; a turn is
//! written again only when it changes (its response lands, an audit verdict
//! arrives), and then without the text already on disk. Within a slot file, a
//! turn's request keeps only the messages it added to an earlier turn's list
//! ([`codec::ReqDelta`], the same for its `msgs` roles), and a streamed reply
//! is stored as the one completion it amounts to ([`codec::assemble_sse`]).
//! Every reference stays inside its slot file, so rotating a slot out never
//! breaks a newer one, and rotated slots are zstd-compressed. A reader folds
//! a key's lines oldest first ([`fold_line`]); v2/v3 lines — one whole
//! snapshot per flush — fold by the same rule.
//!
//! Before v4 every flush rewrote the whole conversation with every turn's full
//! body: a 64-turn agent session was a 20 MB line written 35 times, and 50
//! slots of 10 MB held 37 minutes of traffic with under 25 MB of distinct text.
//!
//! When body-text capture is switched off for a client, key or backend, no text
//! is ever handed to the store; the turn still records its timeline, sizes,
//! status and per-message roles, so the log says what happened even when it
//! cannot say what was said.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::warn;

use crate::lock::LockExt;

pub mod codec;

/// Schema marker of a conversation line. Lines from the old per-request format
/// (`{"dir":"IN"|"OUT", …}`) carry no `v` and are ignored when reading back.
///
/// v3 added the per-turn `audit` flags (see [`crate::audit`]); v2 lines are
/// still read and gain no flags, so a log written before the audit existed keeps
/// opening after an upgrade. v4 lines carry only changed turns, with deltas
/// (see the module docs); v2/v3 lines are whole snapshots and fold the same way.
pub const SCHEMA_V: u64 = 4;

/// Schema versions a reader accepts. A new version is appended, never replaces:
/// an upgrade that silently drops the retained history is the failure mode to
/// avoid here, and additive fields read back as empty on their own.
pub const SCHEMA_READS: [u64; 3] = [2, 3, SCHEMA_V];

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
/// Longest chain of request deltas before a turn's body is stored whole
/// again, so reading one turn back never walks more than this many records.
const MAX_DELTA_DEPTH: u32 = 64;
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
///
/// On disk the request may instead be `req_d`, a delta against an earlier turn
/// of the same slot file; readers resolve it ([`resolve_req`]) before handing
/// text out, so in memory `req_d` is only ever seen between a read and that.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<Arc<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req_d: Option<codec::ReqDelta>,
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
    /// What this turn turned out to be: `Some("compaction")` for a client's
    /// summarization request, `Some("post-compact")` for the turn that resumed
    /// right after one; absent for ordinary turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// How a compaction turn was attached to this conversation (`recency`,
    /// `recency, clear among 3`, `client header`) — display provenance only,
    /// so an inferred link never reads as a stated one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
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
    /// On disk: `msgs` continues the first `msgs_keep` entries of turn
    /// `msgs_base`'s list (same slot file). Resolved away on read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msgs_base: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msgs_keep: Option<usize>,
    /// True when text capture was switched off for this turn's client, key or
    /// backend: the entry is complete, only the text is deliberately absent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
    /// Suspicious-activity flags this turn earned, worst first (`[]` = clean).
    /// They arrive **after** the turn was served: the audit never holds the
    /// reply, so a healthy turn simply carries none. Absent on lines written
    /// before the audit existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audit: Vec<crate::audit::Flag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<TurnContent>,
    /// The store's revision of this turn's last change: what tells the writer
    /// which turns a flush has to carry. Memory only.
    #[serde(skip)]
    pub rev: u64,
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

    /// The header alone: every field but the turns.
    fn head(&self) -> Conv {
        Conv {
            v: self.v,
            key: self.key.clone(),
            user: self.user.clone(),
            session: self.session.clone(),
            session_label: self.session_label.clone(),
            model: self.model.clone(),
            backend: self.backend.clone(),
            priority: self.priority.clone(),
            redacted: self.redacted,
            start_ms: self.start_ms,
            end_ms: self.end_ms,
            dropped_turns: self.dropped_turns,
            turns: Vec::new(),
        }
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
    /// The revision each turn was last written at (by `rid`): a flush carries
    /// only the turns whose [`Turn::rev`] moved past it.
    written: HashMap<u64, u64>,
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
    /// The audit's verdict for a turn that already finished (see
    /// [`crate::audit`]). Lands seconds after the turn was written, so it marks
    /// the conversation dirty again and moves its clock to when the verdict
    /// arrived — that is what makes the re-written line win on read-back.
    Audit {
        key: String,
        rid: u64,
        flags: Vec<crate::audit::Flag>,
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
    /// See [`Turn::kind`] — set where the request was recognized
    /// (see [`crate::classify`]), not here.
    pub kind: Option<String>,
    /// See [`Turn::link`].
    pub link: Option<String>,
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
#[derive(Serialize, Clone)]
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
    /// The worst flags across this conversation's turns, deduplicated per kind
    /// and order, worst first — what a Logs row and the nav count show.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<crate::audit::Flag>,
}

/// The conversation's flags for its row: one entry per (kind, severity) that any
/// of its turns earned, worst first. A row is a summary, not a transcript.
fn worst_flags<'a>(
    lists: impl Iterator<Item = &'a [crate::audit::Flag]>,
) -> Vec<crate::audit::Flag> {
    let mut out: Vec<crate::audit::Flag> = Vec::new();
    for flags in lists {
        for f in flags {
            if !out
                .iter()
                .any(|k: &crate::audit::Flag| k.kind == f.kind && k.bad == f.bad)
            {
                out.push(crate::audit::Flag {
                    kind: f.kind,
                    src: f.src,
                    by: f.by.clone(),
                    prob: f.prob,
                    category: f.category.clone(),
                    detail: f.detail.clone(),
                    excerpt: None,
                    bad: f.bad,
                });
            }
        }
    }
    crate::audit::Flag::sort_worst_first(&mut out);
    out.truncate(4);
    out
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
            written: HashMap::new(),
        })
    }

    /// Record a change to one turn: the conversation is dirty again and the
    /// turn carries the revision the writer compares against.
    fn bump(&mut self, key: &str, rid: u64) {
        let now = now_unix_millis();
        if let Some(e) = self.entries.get_mut(key) {
            e.dirty = true;
            e.rev += 1;
            e.last_ms = now;
            let rev = e.rev;
            if let Some(t) = e.conv.turns.iter_mut().rev().find(|t| t.rid == rid) {
                t.rev = rev;
            }
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
        self.bump(key, rid);
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

    /// Captured text of one turn, hot store only (the facade falls back to
    /// the disk index when this finds nothing).
    fn content(&self, q: &Find, dir: Dir) -> Option<String> {
        let key = self.find(q)?;
        let e = self.entries.get(key)?;
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

    /// The conversations quiet for at least `flush_idle`, quietest first, each
    /// with only the turns changed since they were last written. Cloned out of
    /// the lock (text is shared, not copied) so encoding and the file write
    /// never block the request path.
    fn take_pending(&mut self, flush_idle: Duration) -> Vec<Pending> {
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
            let turns: Vec<Turn> = e
                .conv
                .turns
                .iter()
                .filter(|t| e.written.get(&t.rid) != Some(&t.rev))
                .cloned()
                .collect();
            out.push(Pending {
                head: e.conv.head(),
                turns,
                key,
                rev: e.rev,
            });
        }
        out
    }

    /// A flushed line reached the file: its turns are written at the
    /// revisions it carried, and the conversation is clean unless it changed
    /// while the writer was busy.
    fn mark_written(&mut self, key: &str, turns: &[(u64, u64)], rev: u64) {
        let Some(e) = self.entries.get_mut(key) else { return };
        for (rid, r) in turns {
            e.written.insert(*rid, *r);
        }
        if e.written.len() > e.conv.turns.len() {
            let live: std::collections::HashSet<u64> = e.conv.turns.iter().map(|t| t.rid).collect();
            e.written.retain(|rid, _| live.contains(rid));
        }
        if e.rev == rev {
            e.dirty = false;
        }
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

    /// Every held conversation as the list sees it: its header and per-turn
    /// row facts, ready to merge with what the disk index knows of the key.
    fn rows(&self) -> Vec<(Conv, TurnSums)> {
        self.entries
            .values()
            .map(|e| (e.conv.head(), turn_sums(&e.conv.turns, 0)))
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
    /// The on-disk index of everything the rotation chain holds, so the views
    /// cover the retained history and not just the seeded hot set. `None`
    /// exactly when `file` is.
    disk: Option<Arc<DiskIndex>>,
    /// The newest rid on disk when the log was opened, read synchronously so
    /// [`Self::max_rid`] is right at boot, before the background scan is done.
    boot_rid: u64,
}

struct FileTarget {
    path: String,
    max_bytes: u64,
    max_files: usize,
}

impl ConvLogger {
    /// Open (or create) the log file and start the writer task, which indexes
    /// the rotation chain before its first flush. Nothing is read back into the
    /// hot store: every read folds the key's lines on disk with whatever the
    /// store holds of it, so a resumed conversation appends to its history. A missing parent directory is created: naming a
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
        let disk = Arc::new(DiskIndex::new(&target.path, target.max_files));
        let logger = Self {
            store: Arc::clone(&store),
            file: Some(Arc::clone(&target)),
            boot_rid: disk.newest_rid(),
            disk: Some(Arc::clone(&disk)),
        };
        let handle = tokio::spawn(writer_loop(target, store, disk, flush_idle));
        Ok((logger, handle))
    }

    /// No-op logger for a log that could not be opened.
    pub fn disabled() -> Self {
        Self {
            store: Arc::new(Mutex::new(ConvStore::new())),
            file: None,
            disk: None,
            boot_rid: 0,
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

    /// True while the boot scan of the rotation chain is still filling the
    /// disk index. The hot store already answers for what is live, but the
    /// retained older half is not fully listable yet — the UI says "indexing
    /// the log…" instead of letting a row 404 for a conversation that is on
    /// disk and simply not indexed yet.
    pub fn indexing(&self) -> bool {
        self.disk
            .as_ref()
            .is_some_and(|d| !d.scan_done.load(Ordering::Relaxed))
    }

    /// The oldest moment the conversation chain can still resolve: the
    /// smallest `end_ms` over everything the hot store and the disk index
    /// know. `None` when capture is off or nothing is retained (or indexed)
    /// yet. A Logs row older than this is inert history — clicking it cannot
    /// open, because the line it names has rotated out of the chain.
    pub fn chain_start_ms(&self) -> Option<u64> {
        let mut out: Option<u64> = None;
        {
            let store = self.store.lock_or_recover();
            for e in store.entries.values() {
                out = Some(out.map_or(e.conv.end_ms, |m| m.min(e.conv.end_ms)));
            }
        }
        if let Some(disk) = &self.disk {
            for e in disk.entries.lock_or_recover().values() {
                out = Some(out.map_or(e.head.end_ms, |m| m.min(e.head.end_ms)));
            }
        }
        out
    }

    /// Every turn `rid` the conversation log can resolve right now (hot store
    /// and disk index) as ONE pass: the Logs page marks its rows clickable
    /// from membership in this set instead of probing the log per row (same
    /// single-pass shape as [`Self::max_rid`], which is its boot-only sibling).
    pub fn resolvable_rids(&self) -> std::collections::HashSet<u64> {
        let mut out = std::collections::HashSet::new();
        {
            let store = self.store.lock_or_recover();
            for e in store.entries.values() {
                for t in &e.conv.turns {
                    out.insert(t.rid);
                }
            }
        }
        if let Some(disk) = &self.disk {
            for e in disk.entries.lock_or_recover().values() {
                out.extend(e.turns.keys().copied());
            }
        }
        out
    }

    /// What the rotation chain physically holds: `(bytes on disk across all
    /// slots, unix ms of the oldest slot's last write)`, `None` when capture
    /// is off or no slot file exists yet. The boot retention warning weighs
    /// this against `history_hours` (see [`retention_shortfall`]).
    pub fn chain_disk_stats(&self) -> Option<(u64, u64)> {
        self.disk.as_ref().and_then(|d| d.disk_stats())
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
                    kind: b.kind,
                    link: b.link,
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
                    audit: Vec::new(),
                    content: None,
                    msgs_base: None,
                    msgs_keep: None,
                    rev: 0,
                });
                store.bump(&b.key, b.rid);
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
                store.bump(&key, rid);
            }
            Note::Audit { key, rid, flags } => {
                if let Some(t) = store.turn_mut(&key, rid) {
                    t.audit = flags;
                }
                if let Some(e) = store.entries.get_mut(&key) {
                    // A verdict is an event of its own: the conversation's clock
                    // moves to it, and the turn is written again (without its
                    // text, which is already on disk) to carry the flags.
                    e.conv.end_ms = e.conv.end_ms.max(now_unix_millis());
                }
                store.bump(&key, rid);
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
                }
                store.bump(&key, r.rid);
            }
        }
        store.prune();
    }

    // --- reads (web API) ---

    /// Conversations newest-first, without turns: the hot store first (it
    /// knows what is live), then the disk index for everything older that the
    /// rotation chain still holds.
    pub fn summaries(&self, limit: usize) -> Vec<ConvSummary> {
        // Owned rows, so the store lock is released before the disk index is
        // taken — no lock nesting with the writer.
        let hot = self.store.lock_or_recover().rows();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(hot.len());
        for (mut head, mut turns) in hot {
            // A resumed conversation: the disk holds its earlier turns.
            if let Some((mut disk_head, disk_turns)) =
                self.disk.as_ref().and_then(|d| d.row(&head.key))
            {
                fold_head(&mut disk_head, &head);
                head = disk_head;
                for (rid, t) in disk_turns {
                    turns.entry(rid).or_insert(t);
                }
            }
            seen.insert(head.key.clone());
            out.push(summarize(&head, &turns));
        }
        if let Some(disk) = &self.disk {
            out.extend(disk.summaries(&|k| seen.contains(k)));
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.end_ms));
        out.truncate(limit);
        out
    }

    /// The largest turn `rid` any retained record carries (hot store — seeded
    /// from the newest lines at open — and whatever the disk index has scanned
    /// so far). The rid counter restarts at 1 every boot; a fresh process must
    /// start its counter above this so restored history keeps pointing at the
    /// request its rid names (see [`crate::dispatcher::AppState::seed_rid_floor`]).
    pub fn max_rid(&self) -> u64 {
        let mut m = self.boot_rid;
        {
            let store = self.store.lock_or_recover();
            for e in store.entries.values() {
                for t in &e.conv.turns {
                    m = m.max(t.rid);
                }
            }
        }
        if let Some(disk) = &self.disk {
            let ents = disk.entries.lock_or_recover();
            for e in ents.values() {
                if let Some((r, _)) = e.turns.last_key_value() {
                    m = m.max(*r);
                }
            }
        }
        m
    }

    /// The conversation named by `q`, with captured text stripped (fetch one
    /// turn's text with [`Self::content`]). Hot store first, then disk.
    pub fn index(&self, q: &Find) -> Option<Conv> {
        Some(self.assemble(q)?.without_content())
    }

    /// The whole conversation `q` names: the key's lines on disk folded oldest
    /// first, then the hot store's copy on top — the newest view, and the only
    /// home of turns not flushed yet. The hot store picks the key when it
    /// matches; otherwise the disk index does.
    fn assemble(&self, q: &Find) -> Option<Conv> {
        let hot = {
            let store = self.store.lock_or_recover();
            store.find(q).and_then(|k| store.entries.get(k)).map(|e| e.conv.clone())
        };
        let on_disk = self.disk.as_ref().and_then(|d| {
            let found = match &hot {
                Some(h) => d.find(&Find { key: Some(&h.key), ..Default::default() }),
                None => d.find(q),
            }?;
            d.read(&found)
        });
        let mut conv = match (on_disk, hot) {
            (Some(mut d), Some(h)) => {
                fold_line(&mut d, h);
                d
            }
            (d, h) => d.or(h)?,
        };
        conv.turns.sort_by_key(|t| t.rid);
        Some(conv)
    }

    /// Captured text of one turn (`rid` defaults to the newest turn). Falls
    /// back to the disk, where a request may have to be rebuilt from deltas.
    pub fn content(&self, q: &Find, dir: Dir) -> Option<String> {
        let from_store = self.store.lock_or_recover().content(q, dir);
        if from_store.is_some() {
            return from_store;
        }
        let conv = self.assemble(q)?;
        let rid = match q.rid {
            Some(rid) => rid,
            None => conv.turns.last()?.rid,
        };
        match dir {
            Dir::Req => resolve_req(&conv, rid),
            Dir::Resp => {
                let turn = conv.turns.iter().rev().find(|t| t.rid == rid)?;
                turn.content.as_ref()?.resp.as_deref().map(|s| s.to_string())
            }
        }
    }
}

/// Whether the conversation log's size caps (`cap_bytes` — the operator's
/// `request_log_max_bytes` × the slots `request_log_max_files` keeps) can
/// span `history_hours` of traffic at the write rate the chain has actually
/// been seeing. `used_bytes` is the chain's current size and `oldest_ms` the
/// age of its oldest slot: while the chain is still growing, the rate is
/// measured from its own history; once rotation bites, the surviving age IS
/// the coverage, and the ratio says so either way. A chain younger than a
/// minute is read as one minute old so the first flush cannot read as
/// infinite throughput. Pure so the boot warning's rule is testable without
/// a disk.
pub fn retention_shortfall(
    cap_bytes: u64,
    used_bytes: u64,
    oldest_ms: u64,
    now_ms: u64,
    history_hours: u64,
) -> bool {
    if used_bytes == 0 || cap_bytes == 0 {
        return false;
    }
    let age_hours = (now_ms.saturating_sub(oldest_ms) as f64 / 3_600_000.0).max(1.0 / 60.0);
    let rate = used_bytes as f64 / age_hours;
    rate * history_hours.max(1) as f64 > cap_bytes as f64
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

// ------------------------------------------------------------- folding

/// One line of a slot file as a conversation record, `None` for anything that
/// is not a conversation line of a schema this build reads (the old
/// per-request format, a torn tail).
fn parse_line(raw: &[u8]) -> Option<Conv> {
    let text = std::str::from_utf8(raw).ok()?.trim();
    // Cheap pre-filter before the full parse; `from_str` still decides.
    if !text.starts_with('{') || !text.contains("\"v\":") {
        return None;
    }
    let conv = serde_json::from_str::<Conv>(text).ok()?;
    SCHEMA_READS.contains(&conv.v).then_some(conv)
}

/// Fold a newer header into what is known of a conversation: the clock only
/// widens, the newest model and backend win, the first label sticks and the
/// priority only rises.
fn fold_head(acc: &mut Conv, line: &Conv) {
    acc.v = acc.v.max(line.v);
    if line.start_ms > 0 && (acc.start_ms == 0 || line.start_ms < acc.start_ms) {
        acc.start_ms = line.start_ms;
    }
    acc.end_ms = acc.end_ms.max(line.end_ms);
    if acc.session_label.is_none() {
        acc.session_label = line.session_label.clone();
    }
    if line.model.is_some() {
        acc.model = line.model.clone();
    }
    if line.backend.is_some() {
        acc.backend = line.backend.clone();
    }
    if let Some(p) = &line.priority
        && priority_above(acc.priority.as_deref(), p)
    {
        acc.priority = Some(p.clone());
    }
    acc.redacted |= line.redacted;
    acc.dropped_turns = acc.dropped_turns.max(line.dropped_turns);
}

/// Fold one line of a key into what is known of it so far (lines oldest
/// first; the hot store's copy last). Turns merge by `rid`: a later record of
/// a turn replaces its fields, the text and `msgs` it leaves out (already
/// written) carry over, and a `msgs` delta is resolved against the turn it
/// names. Request deltas stay as they are until [`resolve_req`].
pub fn fold_line(acc: &mut Conv, line: Conv) {
    fold_head(acc, &line);
    for mut t in line.turns {
        if let (Some(base), Some(keep)) = (t.msgs_base.take(), t.msgs_keep.take())
            && let Some(b) = acc.turns.iter().rev().find(|x| x.rid == base)
        {
            let mut msgs = b.msgs[..keep.min(b.msgs.len())].to_vec();
            msgs.append(&mut t.msgs);
            t.msgs = msgs;
        }
        let Some(old) = acc.turns.iter_mut().rev().find(|x| x.rid == t.rid) else {
            acc.turns.push(t);
            continue;
        };
        if t.msgs.is_empty() {
            t.msgs = std::mem::take(&mut old.msgs);
        }
        match (&mut t.content, old.content.take()) {
            (Some(new), Some(prev)) => {
                if new.req.is_none() && new.req_d.is_none() {
                    new.req = prev.req;
                    new.req_d = prev.req_d;
                }
                if new.resp.is_none() {
                    new.resp = prev.resp;
                }
            }
            (None, prev) => t.content = prev,
            (Some(_), None) => {}
        }
        *old = t;
    }
}

/// A rebuilt request: its messages and the object around them.
type ReqParts = Arc<(Vec<Arc<str>>, Arc<str>)>;

/// The messages and template of turn `rid`'s request, following its delta
/// chain back to a whole body. `memo` shares the work across the turns of one
/// conversation, so rebuilding all of them stays linear.
fn req_parts(
    conv: &Conv,
    rid: u64,
    memo: &mut HashMap<u64, Option<ReqParts>>,
    depth: u32,
) -> Option<ReqParts> {
    if let Some(p) = memo.get(&rid) {
        return p.clone();
    }
    // A chain longer than the writer ever makes is a corrupt one (or a cycle).
    if depth > MAX_DELTA_DEPTH * 4 {
        return None;
    }
    let c = conv.turns.iter().rev().find(|t| t.rid == rid)?.content.as_ref()?;
    let out = if let Some(full) = &c.req {
        codec::split_body(full).map(|s| Arc::new((s.msgs, Arc::from(s.tmpl))))
    } else if let Some(d) = &c.req_d {
        req_parts(conv, d.base, memo, depth + 1).and_then(|b| {
            if d.keep > b.0.len() {
                return None;
            }
            let mut msgs = b.0[..d.keep].to_vec();
            msgs.extend(d.add.iter().map(|m| Arc::from(m.get())));
            let tmpl = d.tmpl.as_ref().map_or_else(|| Arc::clone(&b.1), |t| Arc::from(t.get()));
            Some(Arc::new((msgs, tmpl)))
        })
    } else {
        None
    };
    memo.insert(rid, out.clone());
    out
}

/// Turn `rid`'s request body as the client sent it: stored whole, or rebuilt
/// from its deltas (every message byte for byte; the top-level object's
/// separators normalized). `None` when no text was kept, or its base rotated
/// out with an older file.
pub fn resolve_req(conv: &Conv, rid: u64) -> Option<String> {
    let c = conv.turns.iter().rev().find(|t| t.rid == rid)?.content.as_ref()?;
    if let Some(full) = &c.req {
        return Some(full.to_string());
    }
    c.req_d.as_ref()?;
    let p = req_parts(conv, rid, &mut HashMap::new(), 0)?;
    codec::render_body(&p.1, &p.0)
}

/// Rebuild every turn's request in place, so `content.req` is the whole body
/// and no `req_d` is left — the view a script reading the log wants.
pub fn resolve_all(conv: &mut Conv) {
    let mut memo = HashMap::new();
    let rebuilt: Vec<(usize, String)> = conv
        .turns
        .iter()
        .enumerate()
        .filter(|(_, t)| t.content.as_ref().is_some_and(|c| c.req.is_none() && c.req_d.is_some()))
        .filter_map(|(i, t)| {
            let p = req_parts(conv, t.rid, &mut memo, 0)?;
            Some((i, codec::render_body(&p.1, &p.0)?))
        })
        .collect();
    for (i, body) in rebuilt {
        if let Some(c) = conv.turns[i].content.as_mut() {
            c.req = Some(Arc::new(body));
        }
    }
    for c in conv.turns.iter_mut().filter_map(|t| t.content.as_mut()) {
        c.req_d = None;
    }
}

/// Rotation rank of a chain file by its name: the live file is 0, `.N` and
/// `.N.zst` are N + 1 — so sorting by descending rank reads oldest first.
fn chain_rank(path: &Path) -> u64 {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let stem = name.strip_suffix(codec::ZST).unwrap_or(name);
    stem.rsplit_once('.')
        .and_then(|(_, n)| n.parse::<u64>().ok())
        .map_or(0, |n| n + 1)
}

/// Every conversation the given chain files hold, folded across files and
/// with each turn's request rebuilt whole ([`resolve_all`]): what an analysis
/// script or an export wants, whatever schema wrote the lines. Files are read
/// oldest first by their rotation suffix, compressed ones decompressed.
/// Returns the conversations (oldest-first by first sight) and the number of
/// lines that were not conversation records.
pub fn load_conversations(files: &[std::path::PathBuf]) -> std::io::Result<(Vec<Conv>, usize)> {
    let mut files: Vec<&std::path::PathBuf> = files.iter().collect();
    files.sort_by_key(|p| std::cmp::Reverse(chain_rank(p)));
    let mut order: Vec<String> = Vec::new();
    let mut convs: HashMap<String, Conv> = HashMap::new();
    let mut skipped = 0usize;
    for file in files {
        let data = codec::read_slot_file(file)?;
        for raw in data.split(|b| *b == b'\n') {
            if raw.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let Some(line) = parse_line(raw) else {
                skipped += 1;
                continue;
            };
            match convs.get_mut(&line.key) {
                Some(acc) => fold_line(acc, line),
                None => {
                    let mut acc = line.head();
                    order.push(acc.key.clone());
                    fold_line(&mut acc, line);
                    convs.insert(acc.key.clone(), acc);
                }
            }
        }
    }
    let mut out = Vec::with_capacity(order.len());
    for key in order {
        if let Some(mut c) = convs.remove(&key) {
            c.turns.sort_by_key(|t| t.rid);
            resolve_all(&mut c);
            out.push(c);
        }
    }
    Ok((out, skipped))
}

/// The v4 lines a writer would append for `conv` had it flushed once per
/// turn: how a log written by an older build (or exported) is rewritten in
/// the compact form. Expects whole requests (see [`load_conversations`]).
pub fn compact_lines(conv: &Conv) -> Vec<String> {
    let mut st = KeyState::default();
    let mut out = Vec::with_capacity(conv.turns.len());
    for t in &conv.turns {
        let p = Pending {
            key: conv.key.clone(),
            head: conv.head(),
            turns: vec![t.clone()],
            rev: 0,
        };
        if let Ok(line) = serde_json::to_string(&encode(&p, &mut st, 1)) {
            out.push(line);
        }
    }
    out
}

// ------------------------------------------------------------- disk index

/// Where one line of a key lives: `slot` indexes the slot table (0 = the live
/// file, then `.0`, `.1`, …), `offset`/`len` the line within the slot's
/// (decompressed) bytes. Rotation renames and compresses files but never
/// rewrites their content, so a location stays valid while its file merely
/// moves one slot older — which is exactly what
/// [`DiskIndex::shift_for_rotation`] records.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Loc {
    slot: usize,
    offset: u64,
    len: u64,
}

/// What the list needs of one turn, without its text.
#[derive(Clone, Debug)]
struct TurnSum {
    /// Slot of the turn's newest record — the one that decides it.
    slot: usize,
    at_ms: u64,
    open: bool,
    flags: Vec<crate::audit::Flag>,
}

type TurnSums = std::collections::BTreeMap<u64, TurnSum>;

fn turn_sums(turns: &[Turn], slot: usize) -> TurnSums {
    turns
        .iter()
        .map(|t| {
            (
                t.rid,
                TurnSum {
                    slot,
                    at_ms: t.at_ms,
                    open: t.tl.end.is_none(),
                    flags: t.audit.clone(),
                },
            )
        })
        .collect()
}

/// The list view's row for a conversation's header and turns.
fn summarize(head: &Conv, turns: &TurnSums) -> ConvSummary {
    ConvSummary {
        key: head.key.clone(),
        user: head.user.clone(),
        session: head.session.clone(),
        session_label: head.session_label.clone(),
        model: head.model.clone(),
        backend: head.backend.clone(),
        priority: head.priority.clone(),
        turns: turns.len(),
        start_ms: head.start_ms,
        end_ms: head.end_ms,
        redacted: head.redacted,
        live: turns.values().any(|t| t.open),
        dropped_turns: head.dropped_turns,
        flags: worst_flags(turns.values().map(|t| t.flags.as_slice())),
    }
}

/// Everything the index keeps of one key: where its lines are, the folded
/// header, and per-turn row facts.
struct DiskEntry {
    /// Oldest first, the order a read folds them in.
    locs: Vec<Loc>,
    head: Conv,
    turns: TurnSums,
}

/// What a lookup hands back: enough to read the conversation and to answer
/// with its row without holding the index lock.
struct Found {
    locs: Vec<Loc>,
    summary: ConvSummary,
}

/// The on-disk index of every conversation the rotation chain holds — what the
/// list shows and enough to read a conversation back by its lines. Built once
/// at startup by scanning the rotation chain, then maintained by the writer as
/// it appends. It is what makes the Conversations view cover the whole retained
/// file, not just the hot set.
struct DiskIndex {
    /// Slot table, newest first: `[live, .0, .1, …]` (each of the rotated ones
    /// possibly compressed as `<slot>.zst`).
    slots: Vec<String>,
    entries: Mutex<HashMap<String, DiskEntry>>,
    /// Set once the boot scan of the rotation chain has run: until then the
    /// views answer from the hot store only, and the API says so
    /// ([`ConvLogger::indexing`]) rather than 404-ing on lines that are on disk.
    scan_done: AtomicBool,
    /// Decompressed slots recently read, newest last: opening the turns of one
    /// conversation one after another decompresses its file once.
    cache: Mutex<Vec<(usize, Arc<Vec<u8>>)>>,
}

/// Index entries held for at most this many conversations; past it a new key
/// is not indexed (existing ones still update) — a pathological rotation
/// chain costs bounded memory, not unbounded.
const MAX_INDEX_ENTRIES: usize = 200_000;

/// Decompressed slots kept by [`DiskIndex::cache`].
const SLOT_CACHE: usize = 2;

impl DiskIndex {
    fn new(path: &str, max_files: usize) -> Self {
        let mut slots = vec![path.to_string()];
        slots.extend((0..max_files).map(|i| format!("{path}.{i}")));
        Self {
            slots,
            entries: Mutex::new(HashMap::new()),
            scan_done: AtomicBool::new(false),
            cache: Mutex::new(Vec::new()),
        }
    }

    /// The file behind a slot, compressed form first (a slot is compressed
    /// into place and the plain file removed only after, so this never misses
    /// one mid-way), `None` when the slot is empty.
    fn slot_file(&self, slot: usize) -> Option<(std::path::PathBuf, bool)> {
        let base = self.slots.get(slot)?;
        if slot > 0 {
            let zst = std::path::PathBuf::from(format!("{base}{}", codec::ZST));
            if zst.is_file() {
                return Some((zst, true));
            }
        }
        let plain = std::path::PathBuf::from(base);
        plain.is_file().then_some((plain, false))
    }

    /// Total bytes the slots hold **uncompressed** — the quantity
    /// `request_log_max_bytes` caps, so the retention warning compares like with
    /// like — and the last-write time of the oldest existing slot: the chain's
    /// honest reach on disk. Slot mtimes are cheap and never wrong by more than
    /// a rotation interval.
    fn disk_stats(&self) -> Option<(u64, u64)> {
        let mut bytes = 0u64;
        let mut oldest_ms: Option<u64> = None;
        for slot in (0..self.slots.len()).rev() {
            // Oldest slot first: its mtime is the age of the oldest retained
            // data (its last write before it stopped being the live file).
            let Some((file, zst)) = self.slot_file(slot) else {
                continue; // a missing slot is just a shorter chain
            };
            let Ok(m) = std::fs::metadata(&file) else { continue };
            bytes += if zst { codec::zst_content_size(&file).unwrap_or(m.len()) } else { m.len() };
            if oldest_ms.is_none() {
                oldest_ms = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64);
            }
        }
        Some((bytes, oldest_ms?))
    }

    /// The largest turn rid in the newest slot that holds any: rids only grow,
    /// so this is the chain's maximum, at the cost of reading one file
    /// (two right after a rotation) instead of the whole chain.
    fn newest_rid(&self) -> u64 {
        for slot in 0..self.slots.len() {
            let Some(data) = self.slot_file(slot).and_then(|(f, _)| codec::read_slot_file(&f).ok())
            else {
                continue;
            };
            let max = data
                .split(|b| *b == b'\n')
                .filter_map(parse_line)
                .flat_map(|c| c.turns.into_iter().map(|t| t.rid))
                .max();
            if let Some(rid) = max {
                return rid;
            }
        }
        0
    }

    /// A compressed slot's bytes, from the cache or decompressed into it.
    fn slot_bytes(&self, slot: usize, file: &Path) -> Option<Arc<Vec<u8>>> {
        if let Some((_, data)) = self.cache.lock_or_recover().iter().find(|(s, _)| *s == slot) {
            return Some(Arc::clone(data));
        }
        let data = Arc::new(codec::read_slot_file(file).ok()?);
        let mut cache = self.cache.lock_or_recover();
        cache.retain(|(s, _)| *s != slot);
        cache.push((slot, Arc::clone(&data)));
        if cache.len() > SLOT_CACHE {
            cache.remove(0);
        }
        Some(data)
    }

    /// The bytes of one indexed line.
    fn read_at(&self, loc: Loc) -> Option<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let (file, zst) = self.slot_file(loc.slot)?;
        if zst {
            let data = self.slot_bytes(loc.slot, &file)?;
            let start = usize::try_from(loc.offset).ok()?;
            let end = start.checked_add(usize::try_from(loc.len).ok()?)?;
            return data.get(start..end).map(<[u8]>::to_vec);
        }
        let mut f = std::fs::File::open(file).ok()?;
        f.seek(SeekFrom::Start(loc.offset)).ok()?;
        let mut buf = vec![0u8; loc.len as usize];
        f.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    /// Scan the rotation chain oldest-first, so every key's lines are folded
    /// in the order they were written. Runs on the writer task before its first
    /// sweep: until it is done the views answer from the hot store, so an index
    /// still filling costs a little history at the end of the list, never an
    /// outage.
    fn scan(&self) {
        for slot in (0..self.slots.len()).rev() {
            let Some((file, _)) = self.slot_file(slot) else {
                continue; // a missing slot is just a shorter chain
            };
            let Ok(data) = codec::read_slot_file(&file) else {
                continue;
            };
            let mut offset = 0u64;
            for raw in data.split_inclusive(|b| *b == b'\n') {
                let len = raw.len() as u64;
                if let Some(line) = parse_line(raw) {
                    self.record(Loc { slot, offset, len }, &line);
                }
                offset += len;
            }
        }
    }

    /// Fold a line just written (or scanned) into its key's entry.
    fn record(&self, loc: Loc, line: &Conv) {
        let mut entries = self.entries.lock_or_recover();
        if !entries.contains_key(&line.key) && entries.len() >= MAX_INDEX_ENTRIES {
            return;
        }
        let e = entries.entry(line.key.clone()).or_insert_with(|| DiskEntry {
            locs: Vec::new(),
            head: line.head(),
            turns: TurnSums::new(),
        });
        fold_head(&mut e.head, line);
        e.locs.push(loc);
        e.turns.extend(turn_sums(&line.turns, loc.slot));
    }

    /// A rotation renamed every file one slot older and deleted what fell off:
    /// shift every location to follow, dropping what is gone — a turn whose
    /// newest record went with the old file, a key with no line left.
    fn shift_for_rotation(&self) {
        let n = self.slots.len();
        self.cache.lock_or_recover().clear();
        let mut entries = self.entries.lock_or_recover();
        entries.retain(|_, e| {
            e.locs.retain_mut(|l| {
                l.slot += 1;
                l.slot < n
            });
            e.turns.retain(|_, t| {
                t.slot += 1;
                t.slot < n
            });
            if let Some(first) = e.turns.values().map(|t| t.at_ms).min() {
                e.head.start_ms = e.head.start_ms.max(first.min(e.head.end_ms));
            }
            !e.locs.is_empty()
        });
    }

    /// Summaries newest-first, for the list's on-disk half.
    fn summaries(&self, skip: &dyn Fn(&str) -> bool) -> Vec<ConvSummary> {
        self.entries
            .lock_or_recover()
            .iter()
            .filter(|(k, _)| !skip(k))
            .map(|(_, e)| summarize(&e.head, &e.turns))
            .collect()
    }

    /// One key's header and turn facts, for merging with the hot store's row.
    fn row(&self, key: &str) -> Option<(Conv, TurnSums)> {
        let entries = self.entries.lock_or_recover();
        let e = entries.get(key)?;
        Some((e.head.clone(), e.turns.clone()))
    }

    /// The entry `q` names, newest match wins — same matcher as the store,
    /// including that an `rid` query needs the conversation's turn list.
    fn find(&self, q: &Find) -> Option<Found> {
        let entries = self.entries.lock_or_recover();
        let mut best: Option<(u64, &DiskEntry)> = None;
        for (key, e) in entries.iter() {
            let s = &e.head;
            let hit = match q {
                Find { key: Some(want), .. } => *want == key.as_str(),
                Find { rid: Some(rid), session, user, .. } => {
                    e.turns.contains_key(rid)
                        && session.is_none_or(|x| s.session.as_deref() == Some(x))
                        && user.is_none_or(|x| s.user == x)
                }
                Find { key: None, session: Some(x), user, .. } => {
                    s.session.as_deref() == Some(x) && user.is_none_or(|u| s.user == *u)
                }
                Find { key: None, user: Some(u), session: None, rid: None } => s.user == *u,
                _ => false,
            };
            if hit && best.is_none_or(|(ms, _)| s.end_ms > ms) {
                best = Some((s.end_ms, e));
            }
        }
        best.map(|(_, e)| Found {
            locs: e.locs.clone(),
            summary: summarize(&e.head, &e.turns),
        })
    }

    /// Read one indexed conversation back from disk: its lines folded oldest
    /// first, requests left as stored (see [`resolve_req`]). A line that no
    /// longer matches its entry (log truncated or replaced out of band) drops
    /// the entry rather than returning a stranger's conversation — unless a
    /// rotation moved the files between the lookup and the read, which only
    /// makes this answer stale.
    fn read(&self, found: &Found) -> Option<Conv> {
        let key = &found.summary.key;
        let mut acc: Option<Conv> = None;
        for loc in &found.locs {
            let line = self.read_at(*loc).as_deref().and_then(parse_line);
            let Some(line) = line.filter(|l| &l.key == key) else {
                let mut entries = self.entries.lock_or_recover();
                if entries.get(key).is_some_and(|e| e.locs == found.locs) {
                    entries.remove(key);
                }
                return None;
            };
            match acc.as_mut() {
                Some(a) => fold_line(a, line),
                None => {
                    let mut a = line.head();
                    fold_line(&mut a, line);
                    acc = Some(a);
                }
            }
        }
        let mut conv = acc?;
        conv.turns.sort_by_key(|t| t.rid);
        Some(conv)
    }
}

// ------------------------------------------------------------- writer

/// One dirty conversation handed from the store to the writer: its header and
/// the turns changed since they were last written.
struct Pending {
    key: String,
    head: Conv,
    turns: Vec<Turn>,
    /// The conversation's revision when it was taken: if it moved on while
    /// the line was written, it stays dirty.
    rev: u64,
}

/// What the writer remembers of a key it has written into the live file:
/// enough to leave out what that file already holds, and to write a new
/// request as a delta against the last one. Reset whenever the live file is a
/// new one, so no line ever refers outside its own file.
#[derive(Default)]
struct KeyState {
    /// The rotation count this state belongs to.
    epoch: u64,
    /// Turns whose `msgs` are in the file.
    msgs: std::collections::HashSet<u64>,
    /// Turns whose request / response text is in the file, by text identity.
    req: HashMap<u64, (usize, usize)>,
    resp: HashMap<u64, (usize, usize)>,
    /// The last turn whose `msgs` were written, and that list.
    last_msgs: Option<(u64, Vec<MsgMeta>)>,
    last_req: Option<ReqBase>,
}

/// The last request written for a key, as fingerprints.
struct ReqBase {
    rid: u64,
    prints: Vec<(u64, u64)>,
    tmpl: (u64, u64),
    depth: u32,
}

/// Identity of captured text: the same `Arc` is the same capture.
fn ident(text: &Arc<String>) -> (usize, usize) {
    (Arc::as_ptr(text) as usize, text.len())
}

/// The request as a delta against the last one written for its key, when it
/// extends any prefix of that one's messages; `None` stores it whole.
fn encode_req(req: &str, rid: u64, st: &mut KeyState) -> Option<codec::ReqDelta> {
    use serde_json::value::RawValue;
    // Not a chat body (or cut at the content limit): whole, and no base.
    let split = codec::split_body(req)?;
    let prints: Vec<(u64, u64)> = split.msgs.iter().map(|m| codec::fingerprint(m)).collect();
    let tmpl = codec::fingerprint(&split.tmpl);
    let mut out = None;
    let mut depth = 0;
    if let Some(b) = &st.last_req
        && b.rid != rid
        && b.depth < MAX_DELTA_DEPTH
    {
        let keep = codec::common_prefix(&b.prints, &prints);
        let add: Option<Vec<Box<RawValue>>> = split.msgs[keep..]
            .iter()
            .map(|m| RawValue::from_string(m.to_string()).ok())
            .collect();
        let new_tmpl = if tmpl == b.tmpl {
            Some(None)
        } else {
            RawValue::from_string(split.tmpl.clone()).ok().map(Some)
        };
        if keep > 0
            && let (Some(add), Some(tmpl)) = (add, new_tmpl)
        {
            out = Some(codec::ReqDelta { base: b.rid, keep, add, tmpl });
            depth = b.depth + 1;
        }
    }
    st.last_req = Some(ReqBase { rid, prints, tmpl, depth });
    out
}

/// Encode one pending conversation as a v4 line for the live file `epoch`:
/// changed turns only, without the text and `msgs` that file already holds,
/// requests and `msgs` as deltas where they extend an earlier turn, streamed
/// replies folded.
fn encode(p: &Pending, st: &mut KeyState, epoch: u64) -> Conv {
    if st.epoch != epoch {
        *st = KeyState { epoch, ..Default::default() };
    }
    let mut line = p.head.clone();
    line.v = SCHEMA_V;
    for t in &p.turns {
        let mut out = t.clone();
        out.content = None;
        if st.msgs.contains(&t.rid) {
            out.msgs = Vec::new();
        } else {
            if let Some((base, prev)) = &st.last_msgs
                && *base != t.rid
            {
                let keep = codec::common_prefix(prev, &t.msgs);
                if keep > 0 {
                    out.msgs_base = Some(*base);
                    out.msgs_keep = Some(keep);
                    out.msgs = t.msgs[keep..].to_vec();
                }
            }
            st.msgs.insert(t.rid);
            if !t.msgs.is_empty() {
                st.last_msgs = Some((t.rid, t.msgs.clone()));
            }
        }
        if let Some(c) = &t.content {
            let mut kept = TurnContent::default();
            if let Some(req) = &c.req
                && st.req.get(&t.rid) != Some(&ident(req))
            {
                st.req.insert(t.rid, ident(req));
                match encode_req(req, t.rid, st) {
                    Some(d) => kept.req_d = Some(d),
                    None => kept.req = Some(Arc::clone(req)),
                }
            }
            if let Some(resp) = &c.resp
                && st.resp.get(&t.rid) != Some(&ident(resp))
            {
                st.resp.insert(t.rid, ident(resp));
                // An errored turn keeps its stream as captured: that is the
                // evidence of what went wrong.
                let folded = (t.outcome.as_deref() == Some("ok"))
                    .then(|| codec::assemble_sse(resp))
                    .flatten();
                kept.resp = Some(folded.map_or_else(|| Arc::clone(resp), Arc::new));
            }
            if kept.req.is_some() || kept.req_d.is_some() || kept.resp.is_some() {
                out.content = Some(kept);
            }
        }
        line.turns.push(out);
    }
    line
}

/// Writer task: every `SWEEP`, append the conversations that have gone quiet.
async fn writer_loop(
    file: Arc<FileTarget>,
    store: Arc<Mutex<ConvStore>>,
    disk: Arc<DiskIndex>,
    flush_idle: Duration,
) {
    // Index the rotation chain before the first sweep. On this task, not at
    // boot: the hot store answers meanwhile, so a large chain costs early
    // entries of the oldest hour, never a slow start.
    disk.scan();
    disk.scan_done.store(true, Ordering::Relaxed);
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
    // Rotations so far: the identity of the live file, which every delta and
    // every left-out text must stay inside. The live file found at boot is
    // its own generation too — nothing written before this process is known
    // to the writer's key states, so they all start empty.
    let mut epoch: u64 = 1;
    let mut keys: HashMap<String, KeyState> = HashMap::new();
    // Wake at least as often as the flush target, so a short
    // `request_log_flush_secs` is honoured instead of rounded up to the sweep.
    let mut ticker = tokio::time::interval(SWEEP.min(flush_idle.max(Duration::from_millis(10))));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let pending = take_pending(&store, flush_idle);
        for p in pending {
            let mut conv = encode(&p, keys.entry(p.key.clone()).or_default(), epoch);
            let Ok(mut line) = serde_json::to_string(&conv) else {
                warn!("conversation log: failed to serialize {}", p.key);
                keys.remove(&p.key);
                continue;
            };
            if current_size > 0 && current_size + line.len() as u64 + 1 > file.max_bytes {
                // Nothing may hold the old file while it is renamed, or the
                // writer would keep appending to the rotated copy.
                drop(handle.take());
                match rotate(&file.path, file.max_files).await {
                    Ok(()) => {
                        current_size = 0;
                        epoch += 1;
                        // The chain shifted one file older; the entries follow.
                        disk.shift_for_rotation();
                        compress_rotated(&file.path).await;
                        // A fresh file: encode again, so the line refers to
                        // nothing the previous file holds.
                        conv = encode(&p, keys.entry(p.key.clone()).or_default(), epoch);
                        match serde_json::to_string(&conv) {
                            Ok(l) => line = l,
                            Err(_) => {
                                keys.remove(&p.key);
                                continue;
                            }
                        }
                    }
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
            let offset = current_size;
            let written = match append(&mut handle, &file.path, &line).await {
                Ok(n) => {
                    current_size += n;
                    n
                }
                Err(e) => {
                    warn!("conversation log: failed to append to {}: {}", file.path, e);
                    // What the key state says the file holds is now wrong:
                    // start that key over, whole.
                    keys.remove(&p.key);
                    continue;
                }
            };
            // The line is on disk: it is now part of the retained history.
            disk.record(Loc { slot: 0, offset, len: written }, &conv);
            let revs: Vec<(u64, u64)> = p.turns.iter().map(|t| (t.rid, t.rev)).collect();
            store.lock_or_recover().mark_written(&p.key, &revs, p.rev);
        }
        // Forget keys the store no longer holds; a key that comes back starts
        // over whole, which costs bytes, never correctness.
        if keys.len() > MAX_CONVS * 2 {
            let s = store.lock_or_recover();
            keys.retain(|k, _| s.entries.contains_key(k));
        }
    }
}

fn take_pending(store: &Arc<Mutex<ConvStore>>, flush_idle: Duration) -> Vec<Pending> {
    store.lock_or_recover().take_pending(flush_idle)
}

/// Compress the slot a rotation just made (`.0`) in the background pool. A
/// failure leaves it plain, which every reader still opens.
async fn compress_rotated(path: &str) {
    let src = std::path::PathBuf::from(format!("{path}.0"));
    let dst = std::path::PathBuf::from(format!("{path}.0{}", codec::ZST));
    let done = tokio::task::spawn_blocking(move || codec::compress_slot(&src, &dst)).await;
    match done {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("conversation log: could not compress {path}.0 ({e}); it stays uncompressed"),
        Err(e) => warn!("conversation log: compressing {path}.0 failed ({e}); it stays uncompressed"),
    }
}

async fn open_append(path: &str) -> Result<tokio::fs::File, std::io::Error> {
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
}

/// Append one flushed line through the shared handle (reopened when it is not
/// open), so a tailing reader never sees half a line. A failed write drops the
/// handle, so the next call starts from a fresh one. Returns the bytes written.
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

/// Shift the rotation chain: `{path}.{max_files-1}` (plain or `.zst`) is
/// deleted, each existing `{path}.{i}[.zst]` moves up one slot (missing files
/// skipped), and the current file becomes `{path}.0`. Reports the first
/// failure instead of swallowing it: the caller has to keep the file's true
/// size when the chain did not move.
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
    for suffix in ["", codec::ZST] {
        let oldest = format!("{}.{}{suffix}", path, max_files - 1);
        if let Err(e) = tokio::fs::remove_file(&oldest).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            note("cannot drop the oldest file", e, &mut failed);
        }
    }
    for i in (0..max_files.saturating_sub(1)).rev() {
        for suffix in ["", codec::ZST] {
            let from = format!("{path}.{i}{suffix}");
            if tokio::fs::metadata(&from).await.is_ok() {
                let to = format!("{path}.{}{suffix}", i + 1);
                if let Err(e) = tokio::fs::rename(&from, &to).await {
                    note("cannot shift", e, &mut failed);
                }
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

    /// The boot retention warning's rule, without a disk: the chain's own
    /// write rate decides whether its size caps can span `history_hours`.
    #[test]
    fn retention_shortfall_names_a_chain_too_small_for_the_window() {
        let hour = 3_600_000u64;
        let now = 100 * hour;
        // 10 MiB x 6 slots = 60 MiB of chain, 40 MiB written in the last
        // hour: a 24 h window needs ~960 MiB — the caps cannot cover it.
        assert!(retention_shortfall(60 << 20, 40 << 20, now - hour, now, 24));
        // The same caps at a quiet rate cover the window easily.
        assert!(!retention_shortfall(60 << 20, 1 << 20, now - hour, now, 24));
        // A brand-new chain that barely wrote says nothing yet (the one-minute
        // floor keeps the first flush from reading as infinite throughput).
        assert!(!retention_shortfall(60 << 20, 16 << 10, now, now, 24));
        // A full chain that survived only 6 of the 24 requested hours is a
        // shortfall — rotation is eating history faster than the window drains.
        assert!(retention_shortfall(60 << 20, 60 << 20, now - 6 * hour, now, 24));
        // ...and one that survived the whole window does not warn.
        assert!(!retention_shortfall(60 << 20, 60 << 20, now - 25 * hour, now, 24));
    }

    /// A logger without a boot scan (the disabled one) is never "indexing":
    /// there is no chain to scan, and the UI must not wait for nothing.
    #[test]
    fn disabled_logger_is_never_indexing_and_has_no_chain() {
        let log = ConvLogger::disabled();
        assert!(!log.indexing());
        assert_eq!(log.chain_start_ms(), None);
        assert!(log.resolvable_rids().is_empty());
        assert_eq!(log.chain_disk_stats(), None);
    }
    use super::*;

    fn tmp(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("ollamamq_convlog_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        for i in 0..4 {
            let _ = std::fs::remove_file(format!("{}.{}", path.display(), i));
            let _ = std::fs::remove_file(format!("{}.{}.zst", path.display(), i));
        }
        path.to_string_lossy().into_owned()
    }

    /// What the writer would append for the pending conversations: encoded
    /// against `st` as one live file, then marked written in the store.
    fn flush_lines(log: &ConvLogger, st: &mut HashMap<String, KeyState>) -> Vec<(Conv, String)> {
        let pending = take_pending(&log.store, Duration::ZERO);
        let mut out = Vec::new();
        for p in pending {
            let conv = encode(&p, st.entry(p.key.clone()).or_default(), 1);
            let line = serde_json::to_string(&conv).unwrap();
            let revs: Vec<(u64, u64)> = p.turns.iter().map(|t| (t.rid, t.rev)).collect();
            log.store.lock_or_recover().mark_written(&p.key, &revs, p.rev);
            out.push((conv, line));
        }
        out
    }

    fn req_text(key: &str, rid: u64, body: &str) -> Note {
        Note::ReqText {
            key: key.into(),
            rid,
            bytes: Some(body.len() as u64),
            text: Arc::new(body.into()),
        }
    }

    /// Wait for a logger's boot scan of the chain to finish.
    async fn indexed(log: &ConvLogger) {
        for _ in 0..400 {
            if !log.indexing() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the boot scan never finished");
    }

    fn begin(key: &str, rid: u64, at: u64) -> Note {
        Note::Begin(Box::new(Begin {
            kind: None,
            link: None,
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
        let mut st = HashMap::new();
        let lines = flush_lines(&log, &mut st);
        assert_eq!(lines.len(), 1, "one conversation must produce one line");
        let conv: Conv = serde_json::from_str(&lines[0].1).unwrap();
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
        assert!(take_pending(&log.store, Duration::ZERO).is_empty());
        // A late verdict on turn 1 brings back that turn alone, without the
        // text the file already holds.
        log.note(Note::Audit { key: "alice#a1".into(), rid: 1, flags: Vec::new() });
        let again = flush_lines(&log, &mut st);
        assert_eq!(again.len(), 1);
        let conv = &again[0].0;
        assert_eq!(conv.turns.iter().map(|t| t.rid).collect::<Vec<_>>(), vec![1]);
        assert!(conv.turns[0].content.is_none(), "no text written twice");
        assert!(conv.turns[0].msgs.is_empty(), "no msgs written twice");
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
        let line = flush_lines(&log, &mut HashMap::new()).remove(0).1;
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
    async fn resume_appends_only_the_new_turns_and_folds_back_whole() {
        let path = tmp("resume.jsonl");
        {
            let (log, handle) =
                ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_secs(3600)).unwrap();
            let mut st = HashMap::new();
            log.note(begin("carol#c1", 1, 1000));
            log.note(resp("carol#c1", 1, 2000));
            for (_, line) in flush_lines(&log, &mut st) {
                append(&mut None, &path, &line).await.unwrap();
            }
            // The conversation resumes: a second line carries turn 2 alone.
            log.note(begin("carol#c1", 2, 9000));
            log.note(resp("carol#c1", 2, 9500));
            let second = flush_lines(&log, &mut st);
            assert_eq!(second[0].0.turns.len(), 1, "only the new turn is written");
            assert_eq!(second[0].0.turns[0].rid, 2);
            for (_, line) in second {
                append(&mut None, &path, &line).await.unwrap();
            }
            handle.abort();
        }
        // The reader's rule: a key's lines fold oldest first into one
        // conversation holding both turns.
        let (convs, skipped) = load_conversations(&[path.clone().into()]).unwrap();
        assert_eq!((convs.len(), skipped), (1, 0));
        assert_eq!(convs[0].turns.iter().map(|t| t.rid).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!((convs[0].start_ms, convs[0].end_ms), (1000, 9500));
        assert_eq!(convs[0].turns[1].msgs.len(), 1, "msgs survive their delta");
        let _ = std::fs::remove_file(&path);
    }

    /// The point of v4: a session that resends its history stores each
    /// message once, and every turn's body still reads back exactly — from
    /// the file alone, through a fresh process, across a compaction that
    /// broke the prefix.
    #[tokio::test]
    async fn request_deltas_store_each_message_once_and_rebuild_exactly() {
        let path = tmp("delta.jsonl");
        let sys = format!("{{\"role\":\"system\",\"content\":\"{}\"}}", "s".repeat(4000));
        let mut msgs = vec![sys.clone()];
        let mut bodies = Vec::new();
        for i in 0..6u64 {
            msgs.push(format!("{{\"role\":\"user\",\"content\":\"step {i} \\u00e9\"}}"));
            if i == 4 {
                // A compaction: the history is replaced by a summary.
                msgs = vec![sys.clone(), "{\"role\":\"user\",\"content\":\"summary\"}".into()];
            }
            bodies.push(format!(
                "{{\"model\":\"q\",\"messages\":[{}],\"stream\":true,\"tools\":[{{\"type\":\"function\",\"function\":{{\"name\":\"bash\"}}}}]}}",
                msgs.join(",")
            ));
        }
        {
            let (log, handle) =
                ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_secs(3600)).unwrap();
            let mut st = HashMap::new();
            for (i, body) in bodies.iter().enumerate() {
                let rid = 10 + i as u64;
                let mut b = begin("dan#d1", rid, 1000 + rid);
                if let Note::Begin(bx) = &mut b {
                    bx.msgs = msg_meta(body);
                }
                log.note(b);
                log.note(req_text("dan#d1", rid, body));
                log.note(resp("dan#d1", rid, 1500 + rid));
                // One line per turn, as an agent's pauses between turns make.
                for (_, line) in flush_lines(&log, &mut st) {
                    append(&mut None, &path, &line).await.unwrap();
                }
            }
            handle.abort();
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.matches(&"s".repeat(4000)).count(), 1, "the system prompt is stored once");
        assert_eq!(raw.matches("\"req_d\"").count(), 5, "every turn after the first is a delta");
        assert!(raw.len() < bodies.iter().map(String::len).sum::<usize>() / 3);
        // Read back from the file by a script…
        let (convs, _) = load_conversations(&[path.clone().into()]).unwrap();
        for (t, body) in convs[0].turns.iter().zip(&bodies) {
            assert_eq!(t.content.as_ref().unwrap().req.as_deref().map(String::as_str), Some(body.as_str()));
            assert_eq!(t.msgs, msg_meta(body), "msgs rebuilt from their deltas");
        }
        // …and by a fresh process's viewer.
        let (log, handle) =
            ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_secs(3600)).unwrap();
        indexed(&log).await;
        for (i, body) in bodies.iter().enumerate() {
            let q = Find { key: Some("dan#d1"), rid: Some(10 + i as u64), ..Default::default() };
            assert_eq!(log.content(&q, Dir::Req).as_deref(), Some(body.as_str()), "turn {i}");
        }
        let conv = log.index(&find_key("dan#d1")).unwrap();
        assert_eq!(conv.turns.len(), 6);
        assert_eq!(conv.turns[5].msgs, msg_meta(&bodies[5]));
        handle.abort();
        let _ = std::fs::remove_file(&path);
    }

    /// A streamed reply is stored as the completion it amounts to; an errored
    /// one keeps its stream as evidence.
    #[tokio::test]
    async fn streamed_replies_are_folded_unless_the_turn_failed() {
        let path = tmp("sse.jsonl");
        let (log, handle) =
            ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_secs(3600)).unwrap();
        let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi \"}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"there\"},\"finish_reason\":\"stop\"}]}\n\n\
                   data: [DONE]\n\n";
        for (rid, outcome) in [(1u64, "ok"), (2, "backend_error")] {
            log.note(begin("eve#e1", rid, 1000 * rid));
            if let Note::Resp(mut r) = resp("eve#e1", rid, 1000 * rid + 500) {
                r.outcome = outcome.into();
                log.note(Note::Resp(r));
            }
            log.note(Note::RespText {
                key: "eve#e1".into(),
                rid,
                bytes: Some(sse.len() as u64),
                text: Arc::new(sse.into()),
            });
        }
        let (conv, _) = flush_lines(&log, &mut HashMap::new()).remove(0);
        let folded = conv.turns[0].content.as_ref().unwrap().resp.as_deref().unwrap();
        let v: serde_json::Value = serde_json::from_str(folded).unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "hi there");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        let kept = conv.turns[1].content.as_ref().unwrap().resp.as_deref().unwrap();
        assert_eq!(kept.as_str(), sse);
        // The hot store still answers with the stream as captured.
        let q = Find { key: Some("eve#e1"), rid: Some(1), ..Default::default() };
        assert_eq!(log.content(&q, Dir::Resp).as_deref(), Some(sse));
        handle.abort();
        let _ = std::fs::remove_file(&path);
    }

    /// One flushed conversation line for the index tests.
    fn idx_line(key: &str, end_ms: u64, text: &str) -> String {
        serde_json::json!({
            "v": SCHEMA_V, "key": key,
            "user": key.split('#').next().unwrap(),
            "session": key.split('#').nth(1),
            "start_ms": end_ms - 1000, "end_ms": end_ms,
            "turns": [{ "rid": 42, "at_ms": end_ms - 1000, "path": "/v1/chat/completions",
                        "tl": { "end": end_ms },
                        "content": { "req": text } }]
        })
        .to_string()
    }

    fn find_key(key: &str) -> Find<'_> {
        Find { key: Some(key), session: None, user: None, rid: None }
    }

    /// The index's three contracts: newest line per key wins across the chain
    /// (supersede), rotation shifts entries with their files and drops what
    /// falls off, and a (slot, offset, len) actually reads back its line.
    #[test]
    fn disk_index_supersedes_shifts_and_reads() {
        let base = tmp("idx.jsonl");
        // `.0` holds the only view of one conversation and the stale view of
        // another; the live file holds the newer view — written oldest first,
        // exactly as the chain sits on disk.
        std::fs::write(&base, "").unwrap();
        std::fs::write(
            format!("{base}.0"),
            format!(
                "{}\n{}\n",
                idx_line("ann#k1", 5_000, "old view"),
                idx_line("bob#k2", 6_000, "only view")
            ),
        )
        .unwrap();
        std::fs::write(&base, format!("{}\n", idx_line("ann#k1", 9_000, "new view"))).unwrap();
        let idx = DiskIndex::new(&base, 2); // slots: [live, .0, .1]
        idx.scan();

        let sums = idx.summaries(&|_| false);
        assert_eq!(sums.len(), 2, "superseded lines must not double-list");
        let ann = idx.find(&find_key("ann#k1")).expect("ann");
        assert_eq!(ann.summary.end_ms, 9_000, "the live file's newer line wins");
        assert_eq!(ann.locs.iter().map(|l| l.slot).collect::<Vec<_>>(), vec![1, 0], "oldest first");
        let conv = idx.read(&ann).expect("ann reads back");
        assert_eq!(
            conv.turns[0].content.as_ref().unwrap().req.as_deref().map(String::as_str),
            Some("new view"),
            "the newer record of a turn wins the fold"
        );
        // rid matching works off the index without reading the line back.
        let bob = idx
            .find(&Find {
                key: None,
                session: None,
                user: Some("bob"),
                rid: Some(42),
            })
            .expect("bob by rid+user");
        assert_eq!(bob.locs.iter().map(|l| l.slot).collect::<Vec<_>>(), vec![1], "the only view sits in .0");
        let conv = idx.read(&bob).expect("read back");
        assert_eq!(
            conv.turns[0].content.as_ref().unwrap().req.as_deref().map(String::as_str),
            Some("only view")
        );

        // A rotation renames every file one slot older: entries follow; a
        // second rotation drops what moved past the chain's last slot.
        idx.shift_for_rotation();
        assert_eq!(idx.find(&find_key("bob#k2")).unwrap().locs[0].slot, 2);
        idx.shift_for_rotation();
        assert!(idx.find(&find_key("bob#k2")).is_none());
        // ann keeps the line that has not fallen off yet.
        assert_eq!(idx.find(&find_key("ann#k1")).unwrap().locs.len(), 1);
        let _ = std::fs::remove_file(format!("{base}.0"));
    }

    /// The writer's own contract: a flushed line is immediately findable in
    /// the index and reads back from disk — the path that lets the views show
    /// history the hot store dropped.
    #[tokio::test]
    async fn writer_indexes_every_flushed_line() {
        let path = tmp("widx.jsonl");
        let (log, handle) =
            ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_millis(20)).unwrap();
        log.note(begin("erin#e1", 1, 1000));
        log.note(Note::ReqText {
            key: "erin#e1".into(),
            rid: 1,
            bytes: Some(11),
            text: std::sync::Arc::new("hello erin".into()),
        });
        log.note(resp("erin#e1", 1, 2000));
        let disk = log.disk.clone().unwrap();
        let entry = loop {
            if let Some(e) = disk.find(&find_key("erin#e1")) {
                break e;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(entry.locs[0].slot, 0);
        assert_eq!(entry.summary.turns, 1);
        let conv = disk.read(&entry).expect("the flushed line reads back");
        assert_eq!(
            conv.turns[0].content.as_ref().unwrap().req.as_deref().map(String::as_str),
            Some("hello erin")
        );
        handle.abort();
        let _ = std::fs::remove_file(&path);
    }

    /// The rid floor a new process needs must see everything retained:
    /// hot conversations and the disk index loaded at open.
    #[tokio::test]
    async fn max_rid_covers_hot_store_and_disk_index() {
        let path = tmp("maxrid.jsonl");
        let (log, handle) =
            ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_millis(20)).unwrap();
        log.note(begin("erin#e1", 7, 1000));
        log.note(resp("erin#e1", 7, 2000));
        log.note(begin("erin#e1", 13, 3000));
        log.note(resp("erin#e1", 13, 4000));
        let disk = log.disk.clone().unwrap();
        loop {
            if disk
                .find(&find_key("erin#e1"))
                .is_some_and(|e| e.summary.turns >= 2)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(log.max_rid(), 13, "the hot conversation's newest rid");
        handle.abort();
        drop(log);
        // A fresh process: empty hot store, the floor comes off the disk index.
        let (log2, handle2) =
            ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_secs(3600)).unwrap();
        assert_eq!(log2.max_rid(), 13, "the startup disk index carries the rid");
        handle2.abort();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reopening_reads_history_back_and_skips_old_format() {
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
            for (_, line) in flush_lines(&log, &mut HashMap::new()) {
                append(&mut None, &path, &line).await.unwrap();
            }
            handle.abort();
        }
        let (log2, h2) =
            ConvLogger::start(path.clone(), 1 << 20, 1, Duration::from_secs(3600)).unwrap();
        indexed(&log2).await;
        let back = log2.index(&Find { session: Some("d1"), ..Default::default() });
        assert!(back.is_some(), "the line must be read back");
        assert_eq!(back.unwrap().turns.len(), 1);
        // The conversation resumes in the new process: the view is the
        // history on disk plus the new turn, not the new turn alone.
        log2.note(begin("dave#d1", 2, 5000));
        let both = log2.index(&find_key("dave#d1")).unwrap();
        assert_eq!(both.turns.iter().map(|t| t.rid).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(both.start_ms, 1000);
        let row = log2.summaries(10).remove(0);
        assert_eq!((row.turns, row.live), (2, true));
        h2.abort();
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
            let mut all = read_lines(&path);
            for i in 0..2 {
                if let Ok(data) = codec::read_slot_file(Path::new(&format!("{path}.{i}.zst"))) {
                    all.extend(String::from_utf8(data).unwrap().lines().map(|l| serde_json::from_str::<Conv>(l).unwrap()));
                }
            }
            if !all.is_empty() && std::fs::metadata(format!("{path}.0.zst")).is_ok() {
                files = all;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        handle.abort();
        assert!(!files.is_empty(), "no conversation reached the file");
        assert!(
            std::fs::metadata(format!("{path}.0")).is_err(),
            "the rotated slot is replaced by its compressed form"
        );
        for c in &files {
            assert_eq!(c.v, SCHEMA_V);
            assert_eq!(c.turns.len(), 1);
        }
        let cur = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert!(cur <= 1200, "current file {cur}B exceeds the budget");
        // A fresh process opens a turn that lives in a compressed slot.
        let (log2, h2) = ConvLogger::start(path.clone(), 1200, 2, Duration::from_secs(3600)).unwrap();
        indexed(&log2).await;
        let in_zst = log2
            .summaries(100)
            .into_iter()
            .find(|row| {
                log2.disk.as_ref().unwrap().find(&find_key(&row.key)).unwrap().locs[0].slot > 0
            })
            .expect("a conversation sits in a rotated slot");
        let q = Find { key: Some(&in_zst.key), ..Default::default() };
        assert_eq!(log2.content(&q, Dir::Req).as_deref(), Some(body.as_str()));
        h2.abort();
        for i in 0..3 {
            let _ = std::fs::remove_file(format!("{path}.{i}"));
            let _ = std::fs::remove_file(format!("{path}.{i}.zst"));
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn find_prefers_the_newest_conversation_and_honours_rid() {
        let mut store = ConvStore::new();
        for (key, end) in [("a#s", 1000u64), ("b#s", 2000)] {
            store.entry(key, key.split('#').next().unwrap()).conv.end_ms = end;
            store.entries.get_mut(key).unwrap().conv.turns.push(Turn {
                kind: None,
                link: None,
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
                audit: Vec::new(),
                content: None,
                msgs_base: None,
                msgs_keep: None,
                rev: 0,
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
