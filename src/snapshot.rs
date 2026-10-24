//! Point-in-time view of the proxy state shared by the TUI and the web UI.
//!
//! Built in one pass under short-lived locks; every field is an owned copy,
//! so callers can render from it without holding any `AppState` lock.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;

use crate::control::RESULT_VISIBLE;
use crate::dispatcher::{AppState, BackendStatus, LogEvent};
use crate::lock::LockExt;

#[derive(PartialEq, Eq)]
pub struct ActiveOpView {
    pub backend_idx: usize,
    pub verb: String,
    pub model: String,
    pub identifier: Option<String>,
    pub elapsed_secs: u64,
}

#[derive(PartialEq, Eq)]
pub struct RecentResultView {
    pub backend_idx: usize,
    pub ok: bool,
    pub verb: String,
    pub model: String,
    pub identifier: Option<String>,
    pub error: Option<String>,
}

/// One row of the users table.
///
/// The snapshot used to carry five whole `HashMap<String, _>` clones of the
/// per-user bookkeeping and a sixth `Vec` of the ids — six allocations per
/// tracked user, rebuilt ten times a second. Everything the dashboard shows
/// about a user now lives in one row, built in a single pass.
#[derive(PartialEq, Eq)]
pub struct UserRow {
    pub id: String,
    pub queued: usize,
    pub processing: usize,
    pub processed: usize,
    pub dropped: usize,
    pub ip: Option<IpAddr>,
    /// The id is a registered client's name.
    pub registered: bool,
}

impl UserRow {
    /// Requests of this user's that are queued or in flight.
    pub fn outstanding(&self) -> usize {
        self.queued + self.processing
    }
}

/// One row of the TUI's log table: every field the row and its detail header
/// show, plus a display-sized preview of the captured text.
///
/// The full text is deliberately not carried here. `capture()` runs ten times a
/// second and a captured body runs to `log_content_limit` (1 MiB by default), so
/// handing the newest 50 records over whole meant copying — and then deep-
/// comparing, since that is how the dashboard decides whether to redraw — tens of
/// megabytes per second inside the process that holds every queue. The detail
/// view fetches the full text from the conversation log by `rid` when it is
/// opened, which is the same store the web log viewer reads.
#[derive(Clone, Debug, PartialEq)]
pub struct LogRow {
    pub at: std::time::SystemTime,
    /// `"IN"` | `"OUT"` | `"CTL"`.
    pub dir: &'static str,
    pub user: String,
    pub model: Option<String>,
    /// Metrics label of the backend, never a URL.
    pub backend: Option<String>,
    /// Status code or short note ("queued", "dropped", …).
    pub info: String,
    /// First line of the captured text, shortened for the table.
    pub preview: Option<String>,
    /// True when text was captured for this event — the preview can be shorter
    /// than it, or empty when the body itself was.
    pub has_content: bool,
    /// Correlation id: what the detail view looks the full text up by.
    pub rid: Option<u64>,
    pub session: Option<String>,
    pub redacted: bool,
}

impl LogRow {
    fn new(ev: &LogEvent) -> Self {
        let has_content = ev.content.as_deref().is_some_and(|c| !c.is_empty());
        let preview = has_content.then(|| {
            ev.content
                .as_deref()
                .map(String::as_str)
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(30)
                .collect::<String>()
        });
        Self {
            at: ev.at,
            dir: ev.dir,
            user: ev.user.clone(),
            model: ev.model.clone(),
            backend: ev.backend.clone(),
            info: ev.info.clone(),
            preview,
            has_content,
            rid: ev.rid,
            session: ev.session.clone(),
            redacted: ev.redacted,
        }
    }
}

#[derive(PartialEq)]
pub struct StateSnapshot {
    /// Users in display order.
    pub users: Vec<UserRow>,
    /// Request-log records dropped because the log channel was full.
    pub reqlog_dropped: u64,
    pub blocked_ips: HashSet<IpAddr>,
    /// Blocked user ids plus disabled registered clients (blocking a client
    /// disables it, so both read as "blocked" here).
    pub blocked_users: HashSet<String>,
    /// The disabled clients among `blocked_users`.
    pub disabled_clients: HashSet<String>,
    pub vip_user: Option<String>,
    pub boost_user: Option<String>,
    /// Per client, the highest priority level anything of theirs sits at right
    /// now (`"high"` / `"top"`). What the TUI marks, so the marker says what the
    /// board is doing rather than what was pinned once and has since drained.
    pub live_priority: Vec<(String, &'static str)>,
    pub backends: Vec<BackendStatus>,
    /// Per backend (same order as `backends`): `(prefill secs, waste secs)`
    /// accumulated over every request that had measurable cache waste;
    /// `(0.0, 0.0)` where nothing was measurable — the TUI's Waste column
    /// renders `—` for those.
    pub backend_waste: Vec<(f64, f64)>,
    /// Recent request/control events (newest first) for the Logs panel. See
    /// [`LogRow`] for why these are display rows and not the log records.
    pub logs: Vec<LogRow>,
    pub active_ops: Vec<ActiveOpView>,
    pub recent_results: Vec<RecentResultView>,
    /// Which models are served (to mark hidden ones).
    pub model_policy: Arc<crate::model_policy::Policy>,
}

/// Keep the highest level seen for one client (`top` beats `high`, `normal`
/// is not stored at all — an absent client is an ordinary one).
fn note_priority(best: &mut HashMap<String, (&'static str, u8)>, user: &str, level: &'static str) {
    let rank = if level == "top" {
        2
    } else if level == "high" {
        1
    } else {
        0
    };
    if rank > 0 {
        let entry = best.entry(user.to_string()).or_insert((level, rank));
        if rank > entry.1 {
            *entry = (level, rank);
        }
    }
}

pub fn capture(state: &Arc<AppState>, log_limit: usize) -> StateSnapshot {
    let mut users: Vec<UserRow> = {
        let queues = state.queues.lock_or_recover();
        let processing = state.processing_counts.lock_or_recover();
        let processed = state.processed_counts.lock_or_recover();
        let dropped = state.dropped_counts.lock_or_recover();
        let ips = state.user_ips.lock_or_recover();
        queues
            .iter()
            .map(|(id, q)| UserRow {
                queued: q.len(),
                processing: processing.get(id).copied().unwrap_or(0),
                processed: processed.get(id).copied().unwrap_or(0),
                dropped: dropped.get(id).copied().unwrap_or(0),
                ip: ips.get(id).copied(),
                registered: false,
                id: id.clone(),
            })
            .collect()
    };
    // Taken on its own, after the per-user locks are released.
    let (registered, disabled_clients): (HashSet<String>, HashSet<String>) = {
        let clients = state.clients.lock_or_recover();
        (
            clients.names().into_iter().collect(),
            clients.disabled_names().into_iter().collect(),
        )
    };
    for u in &mut users {
        u.registered = registered.contains(&u.id);
    }
    let reqlog_dropped = state.convlog.dropped_count();
    let blocked_ips = state.blocked_ips.lock_or_recover().clone();
    let mut blocked_users = state.blocked_users.lock_or_recover().clone();
    blocked_users.extend(disabled_clients.iter().cloned());
    let vip_user = state.vip_user.lock_or_recover().clone();
    let boost_user = state.boost_user.lock_or_recover().clone();
    // Queued and running requests, per client, reduced to the level they carry.
    let live_priority: Vec<(String, &'static str)> = {
        let mut best: HashMap<String, (&'static str, u8)> = HashMap::new();
        for (user, queue) in state.queues.lock_or_recover().iter() {
            for task in queue {
                note_priority(&mut best, user, task.priority.as_str());
            }
        }
        for run in state.runs.list() {
            note_priority(&mut best, &run.client, run.priority);
        }
        best.into_iter()
            .map(|(user, (level, _))| (user, level))
            .collect()
    };
    let backends = state.backends.lock_or_recover().clone();
    // Waste totals per backend, accumulated as requests finish (see
    // `Stats::backend_waste`): lifetime totals, so the TUI's Waste column no
    // longer resets when a turn ages out of the retained history window.
    let backend_waste: Vec<(f64, f64)> = backends
        .iter()
        .map(|b| {
            state
                .stats
                .backend_waste(&crate::metrics::backend_label(&b.url))
        })
        .collect();
    let logs: Vec<LogRow> = state
        .logs
        .lock_or_recover()
        .iter()
        .rev()
        .take(log_limit)
        .map(LogRow::new)
        .collect();
    let active_ops: Vec<ActiveOpView> = state
        .control_ops
        .lock_or_recover()
        .values()
        .map(|op| ActiveOpView {
            backend_idx: op.backend_idx,
            verb: op.action.verb().to_string(),
            model: op.canonical.clone(),
            identifier: op.identifier.clone(),
            elapsed_secs: op.started.elapsed().as_secs(),
        })
        .collect();
    let recent_results: Vec<RecentResultView> = state
        .control_history
        .lock_or_recover()
        .iter()
        .filter(|r| r.finished_at.elapsed() < RESULT_VISIBLE)
        .map(|r| RecentResultView {
            backend_idx: r.backend_idx,
            ok: r.ok,
            verb: r.action.verb().to_string(),
            model: r.model.clone(),
            identifier: r.identifier.clone(),
            error: r.error.clone(),
        })
        .collect();

    // Busiest first, then most-served, then by id for a stable order.
    users.sort_by(|a, b| {
        b.outstanding()
            .cmp(&a.outstanding())
            .then_with(|| (b.processed + b.dropped).cmp(&(a.processed + a.dropped)))
            .then_with(|| a.id.cmp(&b.id))
    });

    StateSnapshot {
        users,
        reqlog_dropped,
        blocked_ips,
        blocked_users,
        disabled_clients,
        vip_user,
        boost_user,
        live_priority,
        backends,
        backend_waste,
        logs,
        active_ops,
        recent_results,
        model_policy: state.model_policy(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(content: Option<&str>) -> LogEvent {
        LogEvent {
            at: std::time::SystemTime::now(),
            dir: "IN",
            user: "alice".into(),
            model: Some("llama3".into()),
            backend: None,
            info: "POST /api/chat body=123B".into(),
            content: content.map(|c| Arc::new(c.to_string())),
            rid: Some(7),
            session: Some("a3f19c".into()),
            redacted: false,
        }
    }

    #[test]
    fn a_log_row_carries_a_preview_never_the_whole_body() {
        let body = format!("first line\n{}", "x".repeat(200_000));
        let row = LogRow::new(&event(Some(&body)));
        assert!(row.has_content);
        assert_eq!(
            row.preview.as_deref().expect("preview"),
            "first line",
            "the table shows one short line and nothing more"
        );
        assert_eq!(row.rid, Some(7), "the detail view looks the text up by rid");

        // A long single-line body is clipped to the table's width.
        let long = LogRow::new(&event(Some(&"y".repeat(5_000))));
        assert_eq!(long.preview.expect("preview").chars().count(), 30);

        assert!(!LogRow::new(&event(None)).has_content);
        assert!(LogRow::new(&event(None)).preview.is_none());
        let empty = LogRow::new(&event(Some("")));
        assert!(!empty.has_content, "an empty body is not captured content");
    }
}
