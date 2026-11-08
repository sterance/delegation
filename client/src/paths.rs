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

/// Models and llama.cpp runtimes. Separate from `config_dir` because models
/// are large and must not land in a roaming profile:
/// `$XDG_DATA_HOME/delegation-client` (else `~/.local/share/delegation-client`)
/// on Unix, where the Docker image mounts its `client-models` volume, and
/// `%LOCALAPPDATA%\delegation-client` on Windows.
pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("delegation-client")
    }
    #[cfg(not(windows))]
    {
        // The XDG spec says a relative value is invalid and must be ignored.
        let xdg = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        let base = xdg.unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".local").join("share")
        });
        base.join("delegation-client")
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
