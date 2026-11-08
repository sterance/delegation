//! The client has no settings of its own: everything comes from the
//! server's `config.env`, pushed in `welcome`. It caches only the values it
//! needs *before* `welcome` arrives (the handshake timeout and the
//! reconnect backoff), so a restarted client backs off the way its server
//! asked even before it has connected. The CLI keeps them in
//! `cached-timeouts.json` next to its identity file; the tray keeps them per
//! server inside `servers.json`.

use anyhow::{Context, Result};
use protocol::ClientSettings;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The pre-`welcome` part of [`ClientSettings`]. Missing fields default, and
/// unknown ones are ignored, so an older cache that held every setting still
/// loads.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(default)]
pub struct CachedTimeouts {
    pub server_silence_timeout_ms: u64,
    pub reconnect_backoff_initial_ms: u64,
    pub reconnect_backoff_max_ms: u64,
}

impl Default for CachedTimeouts {
    fn default() -> Self {
        Self::from(&ClientSettings::default())
    }
}

impl From<&ClientSettings> for CachedTimeouts {
    fn from(s: &ClientSettings) -> Self {
        CachedTimeouts {
            server_silence_timeout_ms: s.server_silence_timeout_ms,
            reconnect_backoff_initial_ms: s.reconnect_backoff_initial_ms,
            reconnect_backoff_max_ms: s.reconnect_backoff_max_ms,
        }
    }
}

impl CachedTimeouts {
    /// The settings to connect with: these, and defaults for the rest
    /// (which `welcome` replaces before anything uses them).
    pub fn settings(self) -> ClientSettings {
        ClientSettings {
            server_silence_timeout_ms: self.server_silence_timeout_ms,
            reconnect_backoff_initial_ms: self.reconnect_backoff_initial_ms,
            reconnect_backoff_max_ms: self.reconnect_backoff_max_ms,
            ..ClientSettings::default()
        }
    }
}

pub fn cli_path() -> PathBuf {
    crate::paths::config_dir().join("cached-timeouts.json")
}

/// Where earlier versions cached every pushed setting.
fn old_cli_path() -> PathBuf {
    crate::paths::config_dir().join("server-settings.json")
}

/// `None` if there's no cache yet (or it's unreadable): use the defaults.
pub fn load(path: &Path) -> Option<CachedTimeouts> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The CLI's cache, picking up (and then removing) an older
/// `server-settings.json` so it can't be mistaken for a config file.
pub fn load_cli() -> Option<CachedTimeouts> {
    let path = cli_path();
    let old = old_cli_path();
    let cached = load(&path).or_else(|| load(&old));
    if old.exists() {
        if let (Some(timeouts), false) = (&cached, path.exists()) {
            let _ = save(&path, timeouts);
        }
        let _ = std::fs::remove_file(&old);
    }
    cached
}

/// Writes a temp file and renames it over the old one, so a crash
/// mid-write can't leave a half-written cache.
pub fn save(path: &Path, timeouts: &CachedTimeouts) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("creating config dir")?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(timeouts)?).context("writing timeouts cache")?;
    std::fs::rename(&tmp, path).context("replacing timeouts cache")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_missing_or_corrupt_is_none() {
        let dir = std::env::temp_dir().join(format!("delegation-settings-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("cached-timeouts.json");
        assert_eq!(load(&path), None);
        let timeouts = CachedTimeouts {
            reconnect_backoff_max_ms: 60_000,
            ..CachedTimeouts::default()
        };
        save(&path, &timeouts).unwrap();
        assert_eq!(load(&path), Some(timeouts));
        std::fs::write(&path, "{ nope").unwrap();
        assert_eq!(load(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_full_settings_cache_loads_its_timeouts_only() {
        let old = r#"{ "tick_interval_ms": 2000, "sampler_interval_ms": 500, "sample_max_age_ms": 3000,
            "server_silence_timeout_ms": 9000, "reconnect_backoff_initial_ms": 2000,
            "reconnect_backoff_max_ms": 60000, "inference": { "max_tokens": 10 } }"#;
        let cached: CachedTimeouts = serde_json::from_str(old).unwrap();
        assert_eq!(
            cached,
            CachedTimeouts {
                server_silence_timeout_ms: 9_000,
                reconnect_backoff_initial_ms: 2_000,
                reconnect_backoff_max_ms: 60_000,
            }
        );
        let settings = cached.settings();
        assert_eq!(settings.tick_interval_ms, ClientSettings::default().tick_interval_ms);
        assert_eq!(settings.reconnect_backoff_max_ms, 60_000);
    }
}
