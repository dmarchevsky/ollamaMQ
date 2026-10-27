//! Recognizing what a conversation request actually is.
//!
//! A client that runs out of context does not tell the proxy so: it sends an
//! ordinary `chat` completion carrying its own summarization prompt, and that
//! request then looks like a brand-new conversation with an enormous prompt and
//! a cold cache (pi hands every summary call a fresh session id and no
//! `prompt_cache_key`). Nothing on the routes this proxy serves marks it — the
//! two APIs that do (`POST /v1/responses/compact` with `context_management`,
//! and Anthropic's `compact-2026-09-04` beta with a top-level `compaction`
//! parameter) are not routes here. So the marker is the client's own prompt
//! text, and this module's whole job is to find it cheaply and never invent it.
//!
//! Three rules keep the detection honest:
//!
//! * **Whole distinctive phrases, never keywords.** `summarize` alone is
//!   worthless — the git commit-message tool prompt in ordinary traffic says
//!   "Summarize the nature of the changes", and a mislabelled row is worse than
//!   a missing one.
//! * **The phrase must open a message, not merely appear in one.** A request
//!   *is* a summary when the instruction starts a message's content; a request
//!   that only *quotes* one is ordinary traffic. The distinction is structural
//!   and free: text quoted inside a message is JSON-escaped, so a mention reads
//!   `\"content\":\"You are a context summarization assistant…` and can never
//!   match the unescaped `"content":"` boundary a real instruction sits after.
//!   What this buys is paid for in mislabels: an operator debugging the proxy
//!   pasted the pi marker into a `curl` example, every later turn of that
//!   session carried it in a tool call, and the whole conversation filed itself
//!   as compactions.
//! * **A bounded head window.** Every marker below sits in the first message the
//!   client sends (pi's marker sits at byte 73 of the captured body), so a 32 KiB prefix is
//!   enough and a 4 MB body is never scanned. This runs on the request path, on
//!   the same buffer [`crate::dispatcher::derive_session_key`] already
//!   fingerprints.
//!
//! Deliberately **not** detected: OpenAI Codex CLI, which appends its
//! `CONTEXT CHECKPOINT COMPACTION` prompt *after* the history — past any head
//! window, and past the log's content limit.

use std::sync::OnceLock;

use axum::http::HeaderMap;

/// Bytes of a request body examined for a marker.
pub const HEAD_WINDOW: usize = 32 * 1024;

/// What a turn is stored as when it is a client's summarization request.
pub const KIND_COMPACTION: &str = "compaction";
/// The turn that resumed the conversation right after a summary: the one whose
/// prefill is cold because the history was rewritten, which is what makes it
/// worth telling apart from a cache miss the backend caused.
pub const KIND_POST_COMPACT: &str = "post-compact";

/// One recognized summarization prompt: the phrase to look for, named by the
/// comment above it. Chosen to be distinctive whole phrases — see the module
/// docs on why keywords are not enough.
struct Marker {
    phrase: &'static [u8],
    /// Horspool skip table, built on first use.
    skip: OnceLock<[usize; 256]>,
}

const fn marker(phrase: &'static [u8]) -> Marker {
    Marker {
        phrase,
        skip: OnceLock::new(),
    }
}

static MARKERS: [Marker; 3] = [
    // pi, pi-web, OpenClaw (`core/compaction/utils.js`).
    marker(b"You are a context summarization assistant. Your task is to read a conversation"),
    // opencode (`session/prompt/compaction.txt`).
    marker(b"You are a helpful AI assistant tasked with summarizing conversations."),
    // Claude Code, Cline, Roo Code — the summarization turn's instruction.
    marker(b"Your task is to create a detailed summary of the conversation so far"),
];

/// Where a message's text begins in the JSON these clients send: `"content":"`,
/// or `"text":"` for the array form (`"content":[{"type":"text","text":"…"}]`).
/// Where a message's text begins in the JSON these clients send: `"content":`,
/// or `"text":` for the array form (`"content":[{"type":"text","text":"…"}]`).
/// The value's opening quote is deliberately not part of the anchor, so a body
/// that spaces its colon matches as well.
static ANCHORS: [&[u8]; 2] = [b"\"content\":", b"\"text\":"];

/// How much whitespace a marker may be separated from its boundary by: room for
/// a pretty-printed body or a leading newline, nothing more.
const ANCHOR_SLACK: usize = 32;

/// The whitespace a JSON writer may put between a colon and its value.
const WS: [u8; 4] = [b' ', b'\n', b'\t', b'\r'];

fn trim_ws(b: &[u8]) -> &[u8] {
    match b.iter().rposition(|c| !WS.contains(c)) {
        Some(i) => &b[..=i],
        None => &[],
    }
}

/// True when the body *is* a summarization request: one of the known prompts
/// opens one of its messages.
///
/// Runs on the raw body, not on the truncated capture: a client that asked for
/// its log to be redacted is still recognizable (the scan is in memory and
/// stores nothing new).
pub fn is_compaction(body: &[u8]) -> bool {
    let head = &body[..body.len().min(HEAD_WINDOW)];
    MARKERS.iter().any(|m| {
        let mut from = 0usize;
        loop {
            match find(head, from, m.phrase, skip(m)) {
                // Testing what precedes a hit is cheaper than walking every
                // message boundary — and it is exactly what a quoted mention
                // fails, since the quotes around a nested one are escaped.
                Some(at) if opens_a_message(&head[..at]) => return true,
                Some(at) => from = at + 1,
                None => return false,
            }
        }
    })
}

/// True when the bytes before a marker put it at the start of a message's text:
/// `…"content":"` (spaced or not), with at most [`ANCHOR_SLACK`] of whitespace
/// before the phrase. Anywhere else the phrase is the conversation talking
/// *about* a summary — and a copy of one, nested inside a string, reads
/// `\"content\":\"`, which can never end in a boundary.
fn opens_a_message(before: &[u8]) -> bool {
    let tail = &before[before.len().saturating_sub(ANCHOR_SLACK + ANCHORS[0].len() + 2)..];
    let mut tail = trim_ws(tail);
    if tail.last() == Some(&b'"') {
        tail = trim_ws(&tail[..tail.len() - 1]);
    }
    ANCHORS.iter().any(|a| tail.ends_with(a))
}

/// `X-Compaction: 1` — a client that knows it is compacting can say so instead
/// of being recognized. No client sends it today; it is the reliable path for
/// one that wants it, and it always wins over text matching.
pub fn header_flag(headers: &HeaderMap) -> bool {
    headers
        .get("x-compaction")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .is_some_and(|v| {
            !v.is_empty() && !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no")
        })
}

/// `X-Compaction-Of: <session tag>` — the exact parent conversation, from a
/// client that can name it. Wins over every heuristic in [`crate::link`].
pub fn header_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-compaction-of")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn skip(m: &Marker) -> &[usize; 256] {
    m.skip.get_or_init(|| {
        // Chars the phrase never contains shift it whole past them — that skip
        // is the whole reason for the table. Rightmost occurrence wins, and the
        // shift is how far that occurrence sits from the phrase's last char: a
        // shift one too large steps straight over a match.
        let n = m.phrase.len();
        let mut t = [n.max(1); 256];
        for (i, b) in m.phrase[..n.saturating_sub(1)].iter().enumerate() {
            t[*b as usize] = n - 1 - i;
        }
        t
    })
}

/// Boyer-Moore-Horspool substring search over bytes, from an offset, answering
/// where it begins (no `memchr` dependency, and `str::find` cannot be used on a
/// `&[u8]` that may be invalid UTF-8).
fn find(hay: &[u8], from: usize, needle: &[u8], skip: &[usize; 256]) -> Option<usize> {
    let (n, m) = (hay.len(), needle.len());
    if m == 0 || from + m > n {
        return None;
    }
    let mut i = from;
    while i + m <= n {
        if &hay[i..i + m] == needle {
            return Some(i);
        }
        i += skip[hay[i + m - 1] as usize];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The head of a real captured pi compaction request
    /// (`logs/ollamamq-requests.jsonl.2`, 285 694 B), marker at byte 73.
    const PI_BODY: &str = r#"{"model":"Qwen3.8-Flash-Next","messages":[{"role":"developer","content":"You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation."},{"role":"user","content":"<conversation>\n[User]: fix the auth\n"}]}"#;

    #[test]
    fn finds_each_known_prompt() {
        assert!(is_compaction(PI_BODY.as_bytes()));
        for m in MARKERS.iter() {
            let body = format!(
                "{{\"model\":\"m\",\"messages\":[{{\"role\":\"system\",\"content\":\"{}\"}}]}}",
                std::str::from_utf8(m.phrase).unwrap()
            );
            assert!(is_compaction(body.as_bytes()), "{:?}", m.phrase);
        }
    }

    #[test]
    fn finds_a_marker_at_every_offset() {
        // A skip table off by one steps straight over a match, so probe every
        // alignment rather than trusting one lucky position.
        for m in MARKERS.iter() {
            for pad in 0..64 {
                let mut body = vec![b'x'; pad];
                body.extend_from_slice(b"{\"messages\":[{\"content\":\"");
                body.extend_from_slice(m.phrase);
                body.extend_from_slice(b"\",\"more\":1}");
                assert!(
                    is_compaction(&body),
                    "marker at offset {pad} was stepped over"
                );
            }
        }
    }

    #[test]
    fn finds_a_marker_after_pretty_printed_whitespace() {
        let mut body = b"{\n  \"messages\": [\n    {\n      \"content\": \"".to_vec();
        body.extend_from_slice(MARKERS[0].phrase);
        body.extend_from_slice(b"\" }]}");
        assert!(is_compaction(&body));
    }

    /// How this rule was earned: an operator debugging the proxy pasted the pi
    /// summarization prompt into a `curl` example. The tool call stayed in the
    /// conversation, so every later turn of that session carried the marker in
    /// its own history — escaped, one level down, and wrongly filed as a
    /// compaction (bytes taken from that real capture).
    #[test]
    fn a_quoted_marker_is_ordinary_traffic() {
        // One level of escaping, as it arrives on the wire: the marker is there,
        // but only inside a message quoting a curl command.
        let quoted = r#"{"model":"m","messages":[{"role":"system","content":"You are an expert coding assistant."},{"role":"user","content":"I probed the proxy with:\ncurl -d '{\"model\":\"m\",\"messages\":[{\"role\":\"developer\",\"content\":\"You are a context summarization assistant. Your task is to read a conversation and write a compact summary.\"}]}'\nDid it work?"}]}"#;
        assert!(
            quoted.contains(std::str::from_utf8(MARKERS[0].phrase).unwrap()),
            "the fixture must really carry the marker"
        );
        assert!(!is_compaction(quoted.as_bytes()));
    }

    #[test]
    fn finds_a_marker_in_the_array_content_form() {
        let mut body =
            b"{\"messages\":[{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"".to_vec();
        body.extend_from_slice(MARKERS[0].phrase);
        body.extend_from_slice(b"\"}]}]}");
        assert!(is_compaction(&body));
    }

    #[test]
    fn a_marker_buried_in_one_message_is_not_an_instruction() {
        // Not at a message boundary means not an instruction — while a later
        // message that does open with it is still found, because every hit in
        // the window is checked, not just the first.
        let mut deep = b"{\"messages\":[{\"content\":\"".to_vec();
        deep.extend_from_slice(&vec![b'x'; ANCHOR_SLACK + 8]);
        deep.extend_from_slice(MARKERS[0].phrase);
        deep.extend_from_slice(b"\"}]}");
        assert!(!is_compaction(&deep));
    }

    #[test]
    fn ordinary_traffic_is_not_a_compaction() {
        // The false positive that is actually in real traffic: the git
        // commit-message tool prompt, plus prose about compaction.
        let ordinary = br#"{"model":"m","messages":[{"role":"system","content":"You are an expert coding assistant."},{"role":"user","content":"Summarize the nature of the changes (eg. new feature, enhancement to an existing feature) and draft a commit message."},{"role":"user","content":"this repo also does context compaction, summarize the compaction code"}],"tools":[{"function":{"name":"compact_docs"}}]}"#;
        assert!(!is_compaction(ordinary));
    }

    #[test]
    fn ignores_malformed_and_empty_bodies() {
        assert!(!is_compaction(b""));
        assert!(!is_compaction(b"{\"messages\":[{\"role\":\"u\""));
        assert!(!is_compaction(&[0xffu8; 4096]));
    }

    #[test]
    fn only_the_head_window_is_scanned() {
        let marker = b"You are a context summarization assistant. Your task is to read a conversation";
        // A message that opens with the marker, in a body padded past the cut:
        // found — the window is about where scanning stops, not what counts.
        let mut body = b"{\"messages\":[{\"content\":\"".to_vec();
        body.extend_from_slice(marker);
        body.extend_from_slice(&vec![b'a'; HEAD_WINDOW * 2]);
        assert!(is_compaction(&body));
        // The marker ending exactly at the cut: still found; straddling it or
        // beyond it: not. In each case it opens a message — the padding sits in
        // the one before it. A prompt that deep in the body is not a
        // summarization request, and this is the documented cost of a bounded
        // scan (Codex CLI's trailing prompt is the real case).
        let open = b"{\"messages\":[{\"content\":\"";
        let next = b"\"},{\"content\":\"";
        let lead = open.len() + next.len();
        let mut body = open.to_vec();
        body.extend_from_slice(&vec![b'a'; HEAD_WINDOW - lead - marker.len()]);
        body.extend_from_slice(next);
        body.extend_from_slice(marker);
        assert!(is_compaction(&body));
        let mut body = open.to_vec();
        body.extend_from_slice(&vec![b'a'; HEAD_WINDOW - lead - 8]);
        body.extend_from_slice(next);
        body.extend_from_slice(marker);
        assert!(!is_compaction(&body));
        let mut body = open.to_vec();
        body.extend_from_slice(&vec![b'a'; HEAD_WINDOW - lead + 8]);
        body.extend_from_slice(next);
        body.extend_from_slice(marker);
        assert!(!is_compaction(&body));
    }

    #[test]
    fn header_override_is_one_way_and_lenient() {
        let mut h = HeaderMap::new();
        assert!(!header_flag(&h));
        h.insert("x-compaction", "1".parse().unwrap());
        assert!(header_flag(&h));
        h.insert("x-compaction", "true".parse().unwrap());
        assert!(header_flag(&h));
        h.insert("x-compaction", "false".parse().unwrap());
        assert!(!header_flag(&h));
        h.insert("x-compaction", "  ".parse().unwrap());
        assert!(!header_flag(&h));
    }

    #[test]
    fn parent_header_trims_and_rejects_empty() {
        let mut h = HeaderMap::new();
        assert_eq!(header_of(&h), None);
        h.insert("x-compaction-of", " 9c4a1f ".parse().unwrap());
        assert_eq!(header_of(&h).as_deref(), Some("9c4a1f"));
        h.insert("x-compaction-of", "".parse().unwrap());
        assert_eq!(header_of(&h), None);
    }
}
