//! The CLI's cache of the settings its server last pushed in `welcome`,
//! kept next to its identity file so the reconnect backoff follows the
//! server's values even before this run has connected. The tray keeps the
//! same thing per server inside `servers.json` instead.

use anyhow::{Context, Result};
use protocol::ClientSettings;
use std::path::{Path, PathBuf};

pub fn cli_path() -> PathBuf {
    crate::paths::config_dir().join("server-settings.json")
}

/// `None` if there's no cache yet (or it's unreadable): use the defaults.
pub fn load(path: &Path) -> Option<ClientSettings> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Writes a temp file and renames it over the old one, so a crash
/// mid-write can't leave a half-written cache.
pub fn save(path: &Path, settings: &ClientSettings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("creating config dir")?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(settings)?).context("writing settings cache")?;
    std::fs::rename(&tmp, path).context("replacing settings cache")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_missing_or_corrupt_is_none() {
        let dir = std::env::temp_dir().join(format!("delegation-settings-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("server-settings.json");
        assert_eq!(load(&path), None);
        let settings = ClientSettings {
            tick_interval_ms: 2_000,
            ..ClientSettings::default()
        };
        save(&path, &settings).unwrap();
        assert_eq!(load(&path), Some(settings));
        std::fs::write(&path, "{ nope").unwrap();
        assert_eq!(load(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
