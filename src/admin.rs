//! The web UI password set from the UI (`admin.json`, next to `clients.json`).
//!
//! Only an argon2id hash is stored. When the file exists it wins over
//! `OLLAMA_MQ_UI_PASSWORD` and `settings.web_ui.password`: those become the
//! first-run password and the way back in (delete the file and restart).

use std::path::Path;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use serde::{Deserialize, Serialize};

pub const DEFAULT_ADMIN_FILE: &str = "admin.json";

/// Shortest password the UI accepts.
pub const MIN_PASSWORD_CHARS: usize = 12;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdminFile {
    pub username: String,
    /// PHC string (`$argon2id$v=19$...`).
    pub password_hash: String,
    pub changed_at_ms: u64,
}

/// `None` when the file does not exist; an unreadable file is an error, so a
/// damaged file never silently brings back the old password.
pub fn load(path: impl AsRef<Path>) -> Result<Option<AdminFile>, String> {
    let path = path.as_ref();
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let f: AdminFile = serde_json::from_str(&text)
                .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
            PasswordHash::new(&f.password_hash)
                .map_err(|e| format!("{}: bad password hash: {e}", path.display()))?;
            Ok(Some(f))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

pub fn save(path: impl AsRef<Path>, file: &AdminFile) -> Result<(), String> {
    let path = path.as_ref();
    crate::clients::write_json_atomic(path, file)
        .map_err(|e| format!("cannot save {}: {e}", path.display()))
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
        assert!(verify(&h, "correct horse battery"));
        assert!(!verify(&h, "correct horse batterY"));
        assert!(!verify("not a hash", "x"));
        // Salted: the same password hashes differently each time.
        assert_ne!(h, hash("correct horse battery").unwrap());
    }

    #[test]
    fn file_round_trip_and_damage() {
        let dir = std::env::temp_dir().join(format!("omq-admin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("admin.json");
        assert_eq!(load(&path).unwrap(), None);
        let f = AdminFile {
            username: "admin".into(),
            password_hash: hash("a-long-password").unwrap(),
            changed_at_ms: 1,
        };
        save(&path, &f).unwrap();
        assert_eq!(load(&path).unwrap(), Some(f));
        std::fs::write(
            &path,
            r#"{"username":"admin","password_hash":"plain","changed_at_ms":1}"#,
        )
        .unwrap();
        assert!(load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
