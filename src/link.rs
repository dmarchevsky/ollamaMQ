//! Which conversation a compaction request belongs to.
//!
//! A summarization call is not a conversation of its own, but it arrives with no
//! link to the one it summarizes: pi hands it a fresh session id and no
//! `prompt_cache_key`, so the sticky fingerprint produces a brand-new
//! conversation. Attaching it here is what makes it read as one thread on the
//! Activity graph instead of a mystery row with a huge prompt — and it is
//! decided **at request time**, because the attachment also has to supply the
//! parent's scheduling level and backend affinity (a link discovered after the
//! response could not change either).
//!
//! Nothing here ever guesses silently: every attachment carries a
//! `provenance` the UI shows in its tooltip, and the fallback when the answer is
//! not clear is an explicitly **unlinked** compaction row. Ambiguity is normal —
//! one client runs several sessions at once over rotating sockets (a captured pi
//! thread arrived over `:48290` and its summary over `:50302`), and no size or
//! content probe is cheap enough to settle it: the summary body was 285 KB of
//! the 4.3 MB context it summarized, because clients truncate tool results and
//! drop reasoning when they serialize a thread.

use crate::dispatcher::SessionKey;

/// How long ago a thread must have been active to be a candidate parent. A
/// client compacts between turns, so the thread's last request is seconds old.
pub const WINDOW_MS: u64 = 120_000;

/// With several candidates, the newest may stand out on its own: it is recent
/// and the runner-up is far older. Both halves are required, so two threads
/// that are both mid-flight stay unlinked rather than being guessed apart.
pub const CLEAR_WINNER_MS: u64 = 30_000;
pub const CLEAR_WINNER_RATIO: u64 = 3;

/// A finished request that could be the parent of this compaction.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub key: SessionKey,
    /// The level the parent was served at, inherited by the compaction.
    pub priority: &'static str,
    pub at_ms: u64,
    /// This request's own conversation, excluded from its own candidates.
    pub is_self: bool,
    /// A tag from `X-Compaction-Of` matched this candidate exactly.
    pub named: bool,
}

/// The chosen parent, and how it was chosen.
#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    pub key: SessionKey,
    pub priority: &'static str,
    pub provenance: String,
}

/// Pick the parent. `named` is a parent conversation the client itself named
/// (`X-Compaction-Of`): when it is present it must match, because a client that
/// knows the answer and does not match any candidate is telling us something we
/// cannot honour, and a wrong thread is worse than an unlinked row.
pub fn resolve(cands: &[Candidate], named: bool) -> Option<Attachment> {
    let pool: Vec<&Candidate> = cands.iter().filter(|c| !c.is_self).collect();
    if named {
        return pool
            .iter()
            .copied()
            .filter(|c| c.named)
            .max_by_key(|c| c.at_ms)
            .map(|c| attach(c, "client header".into()));
    }
    match pool.len() {
        0 => None,
        1 => Some(attach(pool[0], "recency".into())),
        n => {
            // Newest first; the runner-up is what has to fall clearly behind.
            let best = *pool.iter().max_by_key(|c| c.at_ms)?;
            let second = pool
                .iter()
                .filter(|c| c.at_ms < best.at_ms)
                .max_by_key(|c| c.at_ms)?;
            let now = crate::stats::now_ms();
            let age_best = now.saturating_sub(best.at_ms);
            let age_second = now.saturating_sub(second.at_ms);
            if age_best <= CLEAR_WINNER_MS && age_best.saturating_mul(CLEAR_WINNER_RATIO) <= age_second
            {
                Some(attach(best, format!("recency, clear among {n}")))
            } else {
                None
            }
        }
    }
}

fn attach(c: &Candidate, provenance: String) -> Attachment {
    Attachment {
        key: c.key.clone(),
        priority: c.priority,
        provenance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::now_ms;

    fn key(conv: &str) -> SessionKey {
        SessionKey {
            conversation: conv.into(),
            model: "m".into(),
        }
    }

    fn cand(conv: &str, age_ms: u64, priority: &'static str) -> Candidate {
        Candidate {
            key: key(conv),
            priority,
            at_ms: now_ms().saturating_sub(age_ms),
            is_self: false,
            named: false,
        }
    }

    #[test]
    fn one_candidate_is_the_parent() {
        let c = vec![cand("thread-a", 5_000, "high")];
        let att = resolve(&c, false).unwrap();
        assert_eq!(att.key, key("thread-a"));
        assert_eq!(att.priority, "high");
        assert_eq!(att.provenance, "recency");
    }

    #[test]
    fn clear_winner_wins_among_several() {
        let c = vec![
            cand("old", 90_000, "normal"),
            cand("busy", 2_000, "top"),
            cand("other", 60_000, "normal"),
        ];
        let att = resolve(&c, false).unwrap();
        assert_eq!(att.key, key("busy"));
        assert_eq!(att.priority, "top");
        assert_eq!(att.provenance, "recency, clear among 3");
    }

    #[test]
    fn two_live_threads_stay_unlinked() {
        // Both mid-flight: no honest answer, so no attachment.
        let c = vec![cand("a", 2_000, "normal"), cand("b", 4_000, "normal")];
        assert_eq!(resolve(&c, false), None);
        // Equal timestamps never count as a clear winner either.
        let c = vec![cand("a", 1_000, "normal"), cand("b", 1_000, "normal")];
        assert_eq!(resolve(&c, false), None);
    }

    #[test]
    fn a_stale_but_clear_winner_is_still_too_old() {
        // 3x clearer than the runner-up, but 90s ago: not this compaction's
        // thread — the client compacted between turns, not between sessions.
        let c = vec![cand("a", 90_000, "normal"), cand("b", 119_000, "normal")];
        assert_eq!(resolve(&c, false), None);
    }

    #[test]
    fn nothing_to_attach_to() {
        assert_eq!(resolve(&[], false), None);
        // The compaction's own conversation is never its own parent.
        let mut me = cand("self", 1_000, "normal");
        me.is_self = true;
        assert_eq!(resolve(&[me], false), None);
    }

    #[test]
    fn a_named_parent_must_match() {
        let mut a = cand("a", 5_000, "normal");
        let mut b = cand("b", 1_000, "top");
        a.named = true;
        // `b` is newest, but the client named `a`.
        let att = resolve(&[a.clone(), b.clone()], true).unwrap();
        assert_eq!(att.key, key("a"));
        assert_eq!(att.provenance, "client header");
        // Nothing named: a stale or unknown tag stays unlinked rather than
        // falling back to a guess.
        b.named = false;
        a.named = false;
        assert_eq!(resolve(&[a, b], true), None);
    }
}
