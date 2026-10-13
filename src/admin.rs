//! Argon2id hashing for the web UI password. The hash lives in
//! `settings.web_ui.password_hash` inside `appconf.yaml` (written by the
//! UI's "Change password"); a hash there wins over `OLLAMA_MQ_UI_PASSWORD`
//! and `settings.web_ui.password`, which stay the first-run password and the
//! way back in (delete the hash and restart).

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

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

#[cfg(test)]
mod tests {
    use super::*;

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
