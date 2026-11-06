//! The Windows tray client. See app.rs for the tray itself and form.rs for
//! the "Add server…" window, which runs as a second process of this exe.
//!
//!   delegation-tray                     the tray app
//!   delegation-tray --add-server-form   the form (started by the tray)
//!   delegation-tray --dump-specs        print the hardware report as JSON
//!
//! On other platforms this builds to a stub; use `delegation-client` there.

#![cfg_attr(windows, windows_subsystem = "windows")]

// Compiled everywhere so its tests also run on Linux.
#[cfg_attr(not(windows), allow(dead_code))]
mod store;

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod form;
#[cfg(windows)]
mod menu;

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--add-server-form") => form::run(&args[1..]),
        Some("--dump-specs") => dump_specs(),
        _ => app::run(),
    }
}

/// A GUI-subsystem exe has no console, so when run from a terminal (rather
/// than with its output piped) attach to the parent's to print there.
#[cfg(windows)]
fn dump_specs() {
    use windows::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
    // SAFETY: plain Win32 calls with no pointers.
    unsafe {
        if GetStdHandle(STD_OUTPUT_HANDLE)
            .map(|h| h.is_invalid() || h.0.is_null())
            .unwrap_or(true)
        {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
    match serde_json::to_string_pretty(&delegation_client::hardware::collect()) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("{e}"),
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("delegation-tray is the Windows tray app; on this platform, run delegation-client instead.");
    std::process::exit(1);
}
