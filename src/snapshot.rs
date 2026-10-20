//! Point-in-time view of the proxy state shared by the TUI and the web UI.
//!
//! Built in one pass under short-lived locks; every field is an owned copy,
//! so callers can render from it without holding any `AppState` lock.

use std::collections::HashSet;
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
    pub backends: Vec<BackendStatus>,
    /// Per backend (same order as `backends`): `(prefill secs, waste secs)`
    /// of the retained request history; `(0.0, 0.0)` where nothing was
    /// measurable — the TUI's Waste column renders `—` for those.
    pub backend_waste: Vec<(f64, f64)>,
    /// Recent request/control events (newest first) for the Logs panel.
    pub logs: Vec<LogEvent>,
    pub active_ops: Vec<ActiveOpView>,
    pub recent_results: Vec<RecentResultView>,
    /// Which models are served (to mark hidden ones).
    pub model_policy: Arc<crate::model_policy::Policy>,
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
    let reqlog_dropped = state.reqlog.dropped_count();
    let blocked_ips = state.blocked_ips.lock_or_recover().clone();
    let mut blocked_users = state.blocked_users.lock_or_recover().clone();
    blocked_users.extend(disabled_clients.iter().cloned());
    let vip_user = state.vip_user.lock_or_recover().clone();
    let boost_user = state.boost_user.lock_or_recover().clone();
    let backends = state.backends.lock_or_recover().clone();
    // Waste totals per backend from the retained finish history (the same
    // data the Web overview windows over; the TUI shows the whole history).
    let backend_waste: Vec<(f64, f64)> = {
        let mut acc: Vec<(f64, f64)> = vec![(0.0, 0.0); backends.len()];
        let labels: Vec<String> = backends.iter().map(|b| crate::metrics::backend_label(&b.url)).collect();
        for f in state.stats.finished_since(0) {
            let Some(w) = f.timing.waste_secs else { continue };
            let Some(idx) = f.backend.as_deref().and_then(|l| labels.iter().position(|x| x == l)) else {
                continue;
            };
            if let Some((ps, _)) = f.timing.prefill {
                acc[idx].0 += ps;
            }
            acc[idx].1 += w;
        }
        acc
    };
    let logs: Vec<LogEvent> = state
        .logs
        .lock_or_recover()
        .iter()
        .rev()
        .take(log_limit)
        .cloned()
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
        backends,
        backend_waste,
        logs,
        active_ops,
        recent_results,
        model_policy: state.model_policy(),
    }
}
