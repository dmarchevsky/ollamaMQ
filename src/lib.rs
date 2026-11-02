//! ollamaMQ as a library: the same modules the daemon runs, so a second binary
//! can use them.
//!
//! The proxy itself is `src/main.rs` — a CLI, some wiring, and the router. The
//! code lives here so that `cargo run --example audit_replay`, and any later
//! offline tool over the same data, judges a recorded turn with the *real*
//! scanner instead of a copy that quietly drifts from it. This is not a public
//! API in the usual sense: it is one program's innards, exposed to its own tools.
pub mod admin;
pub mod audit;
pub mod auth;
pub mod classify;
pub mod clients;
pub mod config;
pub mod config_store;
pub mod control;
pub mod dispatcher;
pub mod lock;
pub mod metrics;
pub mod model_policy;
pub mod convlog;
pub mod link;
pub mod session;
pub mod settings;
pub mod snapshot;
pub mod stats;
pub mod web;
