//! Saving `appconf.yaml` back from the web UI: the file is the only settings
//! store, so a UI save rewrites it atomically (tmp file, synced, renamed).
//! Each save keeps the previous version as `appconf.yaml.bak`, which "Undo
//! last save" restores. Saving cannot keep comments — YAML round-trip in Rust
//! has no comment preservation — so the UI warns before it ever writes.

use std::fs;
use std::path::{Path, PathBuf};

use crate::clients::write_store_atomic;
use crate::config::AppConfig;

/// Write `cfg` to `path`, keeping the current file as the backup first.
pub fn save(path: &Path, cfg: &AppConfig) -> Result<(), String> {
    if path.exists() {
        fs::copy(path, backup_path(path))
            .map_err(|e| format!("cannot back up {}: {e}", path.display()))?;
    }
    write_store_atomic(path, cfg).map_err(|e| format!("cannot save {}: {e}", path.display()))
}

pub fn can_undo(path: &Path) -> bool {
    backup_path(path).exists()
}

/// Swap the file and its backup (a second undo redoes) and return the
/// restored config.
pub fn undo(path: &Path, current: &AppConfig) -> Result<AppConfig, String> {
    let bak = backup_path(path);
    let previous: AppConfig = match fs::read_to_string(&bak) {
        Ok(text) => serde_yaml::from_str(&text)
            .map_err(|e| format!("cannot parse {}: {e}", bak.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("nothing to undo".into()),
        Err(e) => return Err(format!("cannot read {}: {e}", bak.display())),
    };
    write_store_atomic(&bak, current).map_err(|e| format!("cannot write {}: {e}", bak.display()))?;
    write_store_atomic(path, &previous)
        .map_err(|e| format!("cannot save {}: {e}", path.display()))?;
    Ok(previous)
}

fn backup_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".bak");
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_timeout(t: u64) -> AppConfig {
        let mut c = AppConfig::default();
        c.settings.timeout = Some(t);
        c
    }

    #[test]
    fn save_undo_round_trip() {
        let dir = std::env::temp_dir().join(format!("omq-confstore-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("appconf.yaml");

        // First save of a file that exists: the original becomes the backup.
        fs::write(&path, "settings:\n  timeout: 50\n").unwrap();
        let mut current = with_timeout(50);
        save(&path, &with_timeout(100)).unwrap();
        assert_eq!(
            crate::config::load_config(path.to_str().unwrap()).unwrap().settings.timeout,
            Some(100)
        );
        assert!(can_undo(&path));
        current.settings.timeout = Some(100);
        let restored = undo(&path, &current).unwrap();
        assert_eq!(restored.settings.timeout, Some(50));
        assert_eq!(
            crate::config::load_config(path.to_str().unwrap()).unwrap().settings.timeout,
            Some(50)
        );
        // Undo again = redo.
        let again = undo(&path, &restored).unwrap();
        assert_eq!(again.settings.timeout, Some(100));

        fs::remove_file(&path).unwrap();
        fs::remove_file(backup_path(&path)).unwrap();
        assert!(!can_undo(&path));
        let _ = fs::remove_dir_all(&dir);
    }
}
