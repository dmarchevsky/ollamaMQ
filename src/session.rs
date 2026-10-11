//! Web UI sign-in sessions and the login attempt limiter.
//!
//! Sessions live in memory: a restart signs everyone out, which is the right
//! failure mode for an admin UI. Tokens are 256-bit random values, carried in
//! an `HttpOnly`, `SameSite=Strict` cookie.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::clients::random_hex;
use crate::lock::LockExt;

/// Name of the session cookie.
pub const COOKIE: &str = "omq_session";

/// How long a sign-in lasts.
pub const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// Sessions kept at once; the oldest are dropped beyond this.
const MAX_SESSIONS: usize = 256;

/// Failed logins allowed per window before further attempts are refused.
const MAX_FAILURES: usize = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(600);

struct Session {
    user: String,
    expires: Instant,
}

#[derive(Default)]
pub struct Sessions {
    inner: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    /// Start a session for `user`; returns its token.
    pub fn create(&self, user: &str) -> String {
        let token = random_hex(32);
        let now = Instant::now();
        let mut map = self.inner.lock_or_recover();
        map.retain(|_, s| s.expires > now);
        if map.len() >= MAX_SESSIONS
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, s)| s.expires)
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
        map.insert(
            token.clone(),
            Session {
                user: user.to_string(),
                expires: now + SESSION_TTL,
            },
        );
        token
    }

    /// The signed-in user for `token`, if the session is live.
    pub fn user(&self, token: &str) -> Option<String> {
        let mut map = self.inner.lock_or_recover();
        match map.get(token) {
            Some(s) if s.expires > Instant::now() => Some(s.user.clone()),
            Some(_) => {
                map.remove(token);
                None
            }
            None => None,
        }
    }

    pub fn remove(&self, token: &str) {
        self.inner.lock_or_recover().remove(token);
    }
}

/// Refuses login attempts after too many failures in a short window, so
/// the password cannot be guessed at speed. Deliberately global rather than
/// per address: behind the reverse proxy every browser has the same peer
/// address anyway, and a correct password is refused too while locked, or
/// the lock would not slow guessing at all.
#[derive(Default)]
pub struct LoginLimiter {
    failures: Mutex<Vec<Instant>>,
}

impl LoginLimiter {
    /// Seconds until attempts are accepted again, or `None` when allowed.
    pub fn locked_for(&self) -> Option<u64> {
        let now = Instant::now();
        let mut f = self.failures.lock_or_recover();
        f.retain(|t| now.duration_since(*t) < FAILURE_WINDOW);
        (f.len() >= MAX_FAILURES).then(|| {
            let oldest = f[0];
            (FAILURE_WINDOW - now.duration_since(oldest))
                .as_secs()
                .max(1)
        })
    }

    pub fn fail(&self) {
        self.failures.lock_or_recover().push(Instant::now());
    }

    pub fn succeed(&self) {
        self.failures.lock_or_recover().clear();
    }
}

/// The value of cookie `name` in a `Cookie` header.
pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_create_lookup_remove() {
        let s = Sessions::default();
        let t = s.create("admin");
        assert_eq!(t.len(), 64);
        assert_eq!(s.user(&t).as_deref(), Some("admin"));
        assert_eq!(s.user("nope"), None);
        s.remove(&t);
        assert_eq!(s.user(&t), None);
    }

    #[test]
    fn limiter_locks_after_failures() {
        let l = LoginLimiter::default();
        for _ in 0..MAX_FAILURES - 1 {
            l.fail();
        }
        assert_eq!(l.locked_for(), None);
        l.fail();
        assert!(l.locked_for().is_some_and(|s| s > 0 && s <= 600));
        l.succeed();
        assert_eq!(l.locked_for(), None);
    }

    #[test]
    fn cookie_parsing() {
        let h = "a=1; omq_session=abc123; b=2";
        assert_eq!(cookie_value(h, COOKIE), Some("abc123"));
        assert_eq!(cookie_value("omq_session_x=1", COOKIE), None);
        assert_eq!(cookie_value("", COOKIE), None);
    }
}
