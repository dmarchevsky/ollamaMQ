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
//! Two rules keep the detection honest:
//!
//! * **Whole distinctive phrases, never keywords.** `summarize` alone is
//!   worthless — the git commit-message tool prompt in ordinary traffic says
//!   "Summarize the nature of the changes", and a mislabelled row is worse than
//!   a missing one.
//! * **A bounded head window.** Every marker below sits in the first message the
//!   client sends (pi's is at byte 83 of the body), so a 32 KiB prefix is
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

/// True when the body carries one of the known summarization prompts.
///
/// Runs on the raw body, not on the truncated capture: a client that asked for
/// its log to be redacted is still recognizable (the scan is in memory and
/// stores nothing new).
pub fn is_compaction(body: &[u8]) -> bool {
    let head = &body[..body.len().min(HEAD_WINDOW)];
    MARKERS.iter().any(|m| contains(head, m.phrase, skip(m)))
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

/// Boyer-Moore-Horspool substring search over bytes (no `memchr` dependency,
/// and `str::find` cannot be used on a `&[u8]` that may be invalid UTF-8).
fn contains(hay: &[u8], needle: &[u8], skip: &[usize; 256]) -> bool {
    let (n, m) = (hay.len(), needle.len());
    if m == 0 || m > n {
        return false;
    }
    let mut i = 0;
    while i + m <= n {
        if &hay[i..i + m] == needle {
            return true;
        }
        i += skip[hay[i + m - 1] as usize];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The head of a real captured pi compaction request
    /// (`logs/ollamamq-requests.jsonl.2`, 285 694 B), marker at byte 83.
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
        // Entirely inside the window: found.
        let mut body = vec![b'a'; HEAD_WINDOW - marker.len()];
        body.extend_from_slice(marker);
        assert!(is_compaction(&body));
        // Straddling the cut, or beyond it: not found. A prompt that deep in the
        // body is not a summarization request, and this is the documented cost
        // of a bounded scan (Codex CLI's trailing prompt is the real case).
        let mut body = vec![b'a'; HEAD_WINDOW + 8];
        body.extend_from_slice(marker);
        assert!(!is_compaction(&body));
        let mut body = vec![b'a'; HEAD_WINDOW - 8];
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
