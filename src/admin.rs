//! Argon2id hashing for the web UI password. The hash lives in
//! `settings.web_ui.password_hash` inside `appconf.yaml` (written by the
//! UI's "Change password"); a hash there wins over `OLLAMA_MQ_UI_PASSWORD`
//! and `settings.web_ui.password`, which stay the first-run password and the
//! way back in (delete the hash and restart).

use argon2::Argon2;
use crate::lock::LockExt;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::extract::State;
use axum::response::IntoResponse;

/// Shortest password the UI accepts.
pub const MIN_PASSWORD_CHARS: usize = 12;

/// Whether `hash` is a parseable PHC string (cheap startup check).
pub fn valid_hash(hash: &str) -> bool {
    PasswordHash::new(hash).is_ok()
}

pub fn hash(password: &str) -> Result<String, String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| format!("no randomness: {e}"))?;
    let salt = SaltString::encode_b64(&salt).map_err(|e| e.to_string())?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

pub fn verify(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| {
        Argon2::default()
            .verify_password(password.as_bytes(), &h)
            .is_ok()
    })
}

/// Who may call `/admin/*`.
///
/// The admin surface changes what the proxy routes and how, so it answers to
/// whoever runs the box — never to whoever happens to reach the port. There is
/// no implicit door: when nothing can identify a caller, `/admin/*` answers 403
/// and the startup log says so. The doors, in the order they are tried:
///
/// 1. the proxy's own key (`OLLAMA_MQ_API_KEY`), when one is set;
/// 2. the dashboard's credentials — a live session cookie, or the same username
///    and password as Basic auth — when a UI password exists; the credential is
///    what counts, not where the dashboard is bound, so an operator who keeps
///    `web_ui.listen` on localhost can still administer over the network;
/// 3. an enabled client key from `keys.toml`, but only when neither door above
///    exists — a client key is what a *client* holds, and a box full of client
///    keys is not a box whose clients may reconfigure it.
pub struct AdminGate {
    api_key: Option<String>,
    web: Option<std::sync::Arc<crate::web::AuthState>>,
}

impl AdminGate {
    pub fn new(
        api_key: Option<String>,
        web: Option<std::sync::Arc<crate::web::AuthState>>,
    ) -> Self {
        Self { api_key, web }
    }

    /// How the gate stands, for the startup log: one line naming what to hand
    /// out (or what to create) before `/admin/*` will answer.
    pub fn describe(&self, clients: usize) -> String {
        let mut doors = Vec::new();
        if self.api_key.is_some() {
            doors.push("the proxy key (OLLAMA_MQ_API_KEY)");
        }
        if self.web.is_some() {
            doors.push("the dashboard login (session cookie or Basic auth)");
        }
        if !doors.is_empty() {
            return format!("open to {}", doors.join(" or "));
        }
        if clients > 0 {
            return "open to any enabled client key from keys.toml — everyone who can send a chat can also reconfigure the proxy; a dashboard password is the credential meant for this".to_string();
        }
        "CLOSED — answering 403 until a dashboard password exists (settings.web_ui.password or OLLAMA_MQ_UI_PASSWORD) or keys.toml has a client".to_string()
    }

    fn key_ok(&self, state: &crate::dispatcher::AppState, key: &str) -> bool {
        if self
            .api_key
            .as_ref()
            .is_some_and(|expected| crate::auth::constant_time_eq(key, expected))
        {
            return true;
        }
        // The client door only exists when nothing stronger does; asking the
        // store also records the key as seen, which is what the proxy's own
        // auth does with the same key on a proxied request.
        self.web.is_none()
            && self.api_key.is_none()
            && state
                .clients
                .lock_or_recover()
                .resolve(key)
                .is_some_and(|c| !c.disabled)
    }

    /// Which door let this request in, or the answer to give when none may.
    pub fn check(
        &self,
        state: &crate::dispatcher::AppState,
        headers: &axum::http::HeaderMap,
    ) -> Result<&'static str, axum::response::Response> {
        if let Some(key) = crate::auth::extract_api_key(headers) && self.key_ok(state, &key) {
            return Ok(if self.api_key.is_some() {
                "proxy key"
            } else {
                "client key"
            });
        }
        if self
            .web
            .as_ref()
            .is_some_and(|auth| crate::web::signed_in(auth, headers))
        {
            return Ok("dashboard login");
        }
        Err(self.refusal(state))
    }

    /// 403, never 401: `WWW-Authenticate` would make browsers pop their own
    /// credentials box (the dashboard avoids it for the same reason), and the
    /// answer has to name what to configure rather than just ask again.
    fn refusal(&self, state: &crate::dispatcher::AppState) -> axum::response::Response {
        let mut doors = Vec::new();
        if self.api_key.is_some() {
            doors.push("the proxy key as `Authorization: Bearer <key>`");
        }
        if self.web.is_some() {
            doors.push("the dashboard login as a session cookie or Basic auth");
        }
        let hint = if !doors.is_empty() {
            format!("sign in with {}", doors.join(" or "))
        } else if !state.clients.lock_or_recover().is_empty() {
            "enable a client key in keys.toml, or set a dashboard password".to_string()
        } else {
            "set a dashboard password (settings.web_ui.password or OLLAMA_MQ_UI_PASSWORD) \
             or add an enabled client key to keys.toml"
                .to_string()
        };
        (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "admin control is closed",
                "hint": hint,
            })),
        )
            .into_response()
    }
}

/// Guards the `/admin/*` routes. Applied to those routes alone: the proxy's own
/// key rule and the admin rule are different questions, and `/admin/*` answers
/// to the second one only.
pub async fn guard(
    State((state, gate)): State<(
        std::sync::Arc<crate::dispatcher::AppState>,
        std::sync::Arc<AdminGate>,
    )>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    match gate.check(&state, request.headers()) {
        Ok(_door) => next.run(request).await,
        Err(res) => res,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::ClientStore;
    use crate::dispatcher::AppState;
    use crate::lock::LockExt;
    use crate::web::{self, WebAuth};
    use axum::http::{HeaderMap, HeaderValue};

    fn state_with_clients(names: &[&str]) -> std::sync::Arc<AppState> {
        let state = std::sync::Arc::new(AppState::new(
            vec![("http://a:1".to_string(), None)],
            5,
            86400,
            60,
            1,
            "appconf.yaml".into(),
            Vec::new(),
            crate::convlog::ConvLogger::disabled(),
            65_536,
            512 * 1024 * 1024,
        ));
        {
            let mut clients = state.clients.lock_or_recover();
            for name in names {
                clients.create_client(name, "").unwrap();
                clients
                    .add_key(name, Some(&format!("{name}-key-0001")), "", false)
                    .unwrap();
            }
        }
        let _ = ClientStore::in_memory;
        state
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn basic(user: &str, pass: &str) -> String {
        use base64::Engine;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
        )
    }

    async fn body(res: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// The whole point: a box with no credential of its own does not hand out
    /// its control plane to whoever reaches the port.
    #[tokio::test]
    async fn nothing_to_check_means_403_naming_what_to_configure() {
        let state = state_with_clients(&[]);
        let gate = AdminGate::new(None, None);
        let err = gate
            .check(&state, &headers(&[]))
            .expect_err("no door is configured");
        assert_eq!(err.status(), axum::http::StatusCode::FORBIDDEN);
        let text = body(err).await;
        assert!(text.contains("admin control is closed"), "{text}");
        assert!(text.contains("password"), "the hint says what to set: {text}");
        assert!(gate.describe(0).starts_with("CLOSED"));
        // A client key does not help either once it is the only door and there
        // are no clients: the answer explains how to make one.
        let with_client = state_with_clients(&["alice"]);
        let err = gate
            .check(&with_client, &headers(&[("authorization", "Bearer nope")]))
            .expect_err("a wrong key is not a door");
        assert_eq!(err.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_proxy_key_is_enough() {
        let state = state_with_clients(&[]);
        let gate = AdminGate::new(Some("proxy-key".into()), None);
        assert!(
            gate.check(&state, &headers(&[("authorization", "Bearer proxy-key")]))
                .is_ok()
        );
        assert!(
            gate.check(
                &state,
                &headers(&[("x-api-key", "proxy-key")]),
            )
            .is_ok()
        );
        assert!(
            gate.check(&state, &headers(&[("authorization", "Bearer other")]))
                .is_err()
        );
        assert!(gate.describe(0).contains("OLLAMA_MQ_API_KEY"));
    }

    /// The dashboard's own credential — cookie or the same username and password
    /// as Basic auth, and not a client key.
    #[tokio::test]
    async fn the_dashboard_login_is_enough() {
        let state = state_with_clients(&["alice"]);
        let auth = web::auth_state(WebAuth::plain("admin", "a-long-enough-password"));
        let gate = AdminGate::new(None, Some(auth.clone()));

        assert!(
            gate.check(
                &state,
                &headers(&[("authorization", &basic("admin", "a-long-enough-password"))])
            )
            .is_ok()
        );
        assert!(
            gate.check(
                &state,
                &headers(&[("authorization", &basic("admin", "wrong"))])
            )
            .is_err()
        );

        let token = web::test_session(&auth);
        let cookie = format!("{}={token}", crate::session::COOKIE);
        assert!(
            gate.check(&state, &headers(&[("cookie", &cookie)])).is_ok(),
            "a signed-in browser is the operator"
        );
        // The client key that drives the chat is not enough while a dashboard
        // credential exists.
        assert!(
            gate.check(
                &state,
                &headers(&[("authorization", "Bearer alice-key-0001")])
            )
            .is_err()
        );
    }

    /// With no credential of its own the box still has to be drivable, and the
    /// only thing it can compare against is the key its clients already hold.
    #[tokio::test]
    async fn a_client_key_drives_admin_only_when_nothing_better_exists() {
        let state = state_with_clients(&["alice"]);
        let gate = AdminGate::new(None, None);
        assert_eq!(
            gate.check(&state, &headers(&[("authorization", "Bearer alice-key-0001")])).ok(),
            Some("client key")
        );
        assert_eq!(
            gate.check(&state, &headers(&[("x-api-key", "alice-key-0001")])).ok(),
            Some("client key")
        );
        // The same key is refused as soon as the operator has a credential: a
        // client must not be able to reconfigure the box it chats to.
        let with_ui = AdminGate::new(
            None,
            Some(web::auth_state(WebAuth::plain("admin", "a-long-enough-password"))),
        );
        assert!(
            with_ui
                .check(&state, &headers(&[("authorization", "Bearer alice-key-0001")]))
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_disabled_client_key_is_not_a_door() {
        let state = state_with_clients(&["bob"]);
        state
            .clients
            .lock_or_recover()
            .set_disabled("bob", true)
            .unwrap();
        let gate = AdminGate::new(None, None);
        assert!(
            gate.check(&state, &headers(&[("authorization", "Bearer bob-key-0001")]))
                .is_err()
        );
    }

    #[test]
    fn hash_verifies_only_its_password() {
        let h = hash("correct horse battery").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(valid_hash(&h));
        assert!(!valid_hash("not a hash"));
        assert!(verify(&h, "correct horse battery"));
        assert!(!verify(&h, "correct horse batterY"));
        assert!(!verify("not a hash", "x"));
        // Salted: the same password hashes differently each time.
        assert_ne!(h, hash("correct horse battery").unwrap());
    }
}
