//! A minimal `log` backend that keeps the `[ts] [tag] msg` line format the
//! client has always printed. The tag is the log target, so call sites
//! write `log::info!(target: "ws", "...")`.
//!
//! The CLI logs to stdout (warnings and errors to stderr, as before); the
//! tray app has no console, so it logs to a file instead.
//!
//! `DELEGATION_LOG=debug` adds the debug lines, among them everything
//! `llama-server` prints.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

enum Sink {
    Stdout,
    File(Mutex<File>),
}

struct Logger(Sink, Level);

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.1
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!("[{}] [{}] {}", protocol::ts(), record.target(), record.args());
        match &self.0 {
            Sink::Stdout if record.level() <= Level::Warn => eprintln!("{line}"),
            Sink::Stdout => println!("{line}"),
            Sink::File(file) => {
                if let Ok(mut f) = file.lock() {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
    }

    fn flush(&self) {
        if let Sink::File(file) = &self.0 {
            if let Ok(mut f) = file.lock() {
                let _ = f.flush();
            }
        }
    }
}

pub fn init_stdout() {
    install(Sink::Stdout);
}

/// Appends to `path`, first moving a log over 1 MB aside to `<path>.old`
/// so it can't grow forever.
pub fn init_file(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    install(Sink::File(Mutex::new(file)));
    Ok(())
}

fn install(sink: Sink) {
    let debug = std::env::var("DELEGATION_LOG").is_ok_and(|v| v.eq_ignore_ascii_case("debug"));
    let (level, filter) = if debug {
        (Level::Debug, LevelFilter::Debug)
    } else {
        (Level::Info, LevelFilter::Info)
    };
    if log::set_boxed_logger(Box::new(Logger(sink, level))).is_ok() {
        log::set_max_level(filter);
    }
}
