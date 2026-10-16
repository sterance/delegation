//! Where the client keeps its files.

use std::path::PathBuf;

/// Identity and config. Unix keeps `$HOME/.config/delegation-client`, the
/// path the Docker image mounts its identity volume at; Windows uses
/// `%APPDATA%\delegation-client`.
pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("delegation-client")
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config").join("delegation-client")
    }
}

/// Log files (the tray app only; the CLI logs to stdout). Kept out of the
/// roaming profile on Windows: `%LOCALAPPDATA%\delegation-client`.
pub fn log_dir() -> PathBuf {
    #[cfg(windows)]
    {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("delegation-client")
    }
    #[cfg(not(windows))]
    {
        config_dir()
    }
}
