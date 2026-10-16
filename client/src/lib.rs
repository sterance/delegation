//! The delegation client as a library: enrollment, the session controller
//! and hardware collection. Two front ends use it: the headless CLI
//! (`bin/delegation-client.rs`, used natively on Linux and in Docker) and
//! the Windows tray app (`bin/delegation-tray/`).

pub mod hardware;
pub mod identity;
pub mod log;
pub mod paths;
pub mod session;

/// This machine's hostname, read fresh each call so a rename shows up on
/// the next connection.
pub fn hostname() -> String {
    gethostname::gethostname()
        .into_string()
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}
