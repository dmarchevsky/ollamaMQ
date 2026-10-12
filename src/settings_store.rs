//! Settings changed in the web UI (`settings.json`, next to `clients.json`).
//!
//! `appconf.yaml` stays the hand-edited starting point and is never written:
//! the UI saves only what it changed here, and every value here wins over the
//! file's (see [`crate::config::Settings::overlay`]). Each save keeps the
//! previous version as `settings.json.bak`, which "Undo last save" restores.

use std::fs;
use std::path::{Path, PathBuf};

use crate::clients::write_json_atomic;
use crate::config::UiOverlay;

pub const DEFAULT_SETTINGS_FILE: &str = "settings.json";

pub struct SettingsStore {
    /// `None` keeps everything in memory (tests).
    path: Option<PathBuf>,
    overlay: UiOverlay,
}

impl SettingsStore {
    pub fn in_memory() -> Self {
        Self {
            path: None,
            overlay: UiOverlay::default(),
        }
    }

    /// Load from `path`; a missing file is an empty overlay. A file that
    /// exists but cannot be parsed is an error, so a typo never silently
    /// drops every setting made in the UI.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let overlay = read(&path)?.unwrap_or_default();
        Ok(Self {
            path: Some(path),
            overlay,
        })
    }

    pub fn overlay(&self) -> &UiOverlay {
        &self.overlay
    }

    /// Replace the overlay, keeping the current file as the backup.
    pub fn save(&mut self, next: UiOverlay) -> Result<(), String> {
        if let Some(path) = &self.path {
            if path.exists() {
                fs::copy(path, backup_path(path))
                    .map_err(|e| format!("cannot back up {}: {e}", path.display()))?;
            } else {
                // Undo after the first save goes back to "nothing set".
                write_json_atomic(&backup_path(path), &UiOverlay::default())
                    .map_err(|e| format!("cannot write {}: {e}", backup_path(path).display()))?;
            }
            write_json_atomic(path, &next)
                .map_err(|e| format!("cannot save {}: {e}", path.display()))?;
        }
        self.overlay = next;
        Ok(())
    }

    pub fn can_undo(&self) -> bool {
        self.path.as_ref().is_some_and(|p| backup_path(p).exists())
    }

    /// Swap in the backup; the current version becomes the backup, so a
    /// second undo redoes.
    pub fn undo(&mut self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Err("nothing to undo".into());
        };
        let bak = backup_path(path);
        let previous = read(&bak)?.ok_or("nothing to undo")?;
        write_json_atomic(&bak, &self.overlay)
            .map_err(|e| format!("cannot write {}: {e}", bak.display()))?;
        write_json_atomic(path, &previous)
            .map_err(|e| format!("cannot save {}: {e}", path.display()))?;
        self.overlay = previous;
        Ok(())
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".bak");
    PathBuf::from(p)
}

fn read(path: &Path) -> Result<Option<UiOverlay>, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("cannot parse {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_timeout(t: u64) -> UiOverlay {
        let mut o = UiOverlay::default();
        o.settings.timeout = Some(t);
        o
    }

    #[test]
    fn save_load_and_undo_round_trip() {
        let dir = std::env::temp_dir().join(format!("omq-settings-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        let mut s = SettingsStore::load(&path).unwrap();
        assert_eq!(s.overlay(), &UiOverlay::default());
        assert!(!s.can_undo());

        s.save(with_timeout(100)).unwrap();
        s.save(with_timeout(200)).unwrap();
        assert_eq!(
            SettingsStore::load(&path).unwrap().overlay(),
            &with_timeout(200)
        );

        s.undo().unwrap();
        assert_eq!(s.overlay(), &with_timeout(100));
        assert_eq!(
            SettingsStore::load(&path).unwrap().overlay(),
            &with_timeout(100)
        );
        // Undo again = redo.
        s.undo().unwrap();
        assert_eq!(s.overlay(), &with_timeout(200));

        fs::write(&path, "{not json").unwrap();
        assert!(SettingsStore::load(&path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_save_can_be_undone_to_nothing() {
        let dir = std::env::temp_dir().join(format!("omq-settings-first-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut s = SettingsStore::load(dir.join("settings.json")).unwrap();
        s.save(with_timeout(5)).unwrap();
        s.undo().unwrap();
        assert_eq!(s.overlay(), &UiOverlay::default());
        let _ = fs::remove_dir_all(&dir);
    }
}
