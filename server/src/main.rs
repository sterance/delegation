mod access;
mod auth;
mod chat;
mod clients;
mod config;
mod conversations;
mod db;
mod http;
mod live;
mod metrics;
mod models;
mod ratelimit;
mod replies;
mod routes;
#[cfg(test)]
mod tests;
mod ws;

use auth::Auth;
use config::{ConfigFile, Settings};
use db::Db;
use live::Live;
use models::catalog::Catalog;
use models::commands::ModelCommands;
use replies::ReplyRunner;
use std::io::{BufRead, IsTerminal};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, SystemTime};
use tokio::time::MissedTickBehavior;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    /// Connections, metric history and unflushed `last_seen_at` values.
    pub live: Arc<Live>,
    pub settings: Arc<Settings>,
    /// Sessions cache, rate limits, password hashing, the setup token.
    pub auth: Arc<Auth>,
    /// Every chat reply being generated.
    pub replies: Arc<ReplyRunner>,
    /// The models clients may be told to install.
    pub catalog: Arc<Catalog>,
    /// Model commands sent to clients, recent ones included.
    pub model_commands: Arc<ModelCommands>,
}

static DEBUG_LOGGING: AtomicBool = AtomicBool::new(false);

pub fn log(msg: impl AsRef<str>) {
    println!("[{}] {}", protocol::ts(), msg.as_ref());
}

/// Per-tick, per-sample and per-flush detail, shown with
/// `SERVER_LOG_LEVEL=debug`. Takes a closure so nothing is formatted when
/// it's off.
pub fn debug(msg: impl FnOnce() -> String) {
    if DEBUG_LOGGING.load(Ordering::Relaxed) {
        println!("[{}] {}", protocol::ts(), msg());
    }
}

const USAGE: &str = "usage:
  delegation-server                         run the server
  delegation-server reset-admin-password    set a new password for the admin account
                                            (prompts, or reads one line from stdin)
  delegation-server backup [FILE]           write a consistent copy of the database
                                            (VACUUM INTO), default next to it";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("serve") => {
            let Some(settings) = load_settings(true) else {
                return ExitCode::FAILURE;
            };
            let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => {
                    eprintln!("failed to start the async runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            runtime.block_on(serve(settings))
        }
        Some("reset-admin-password") if args.len() == 1 => match load_settings(false) {
            Some(settings) => reset_admin_password(&settings),
            None => ExitCode::FAILURE,
        },
        Some("backup") if args.len() <= 2 => match load_settings(false) {
            Some(settings) => backup(&settings, args.get(1).map(String::as_str)),
            None => ExitCode::FAILURE,
        },
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(_) => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// `config.env` plus the environment, validated. None (after printing
/// every problem) if anything is invalid.
fn load_settings(verbose: bool) -> Option<Settings> {
    let file = ConfigFile::load(&["config.env".into()]);
    if verbose {
        match file.source() {
            Some(path) => log(format!("[config] loaded {}", path.display())),
            None => log("[config] no config file, using environment and defaults"),
        }
    }
    for warning in config::replaced_keys_in_use(|key| file.get(key)) {
        log(format!("[config] warning: {warning}"));
    }
    match Settings::parse(|key| file.get(key)) {
        Ok(settings) => Some(settings),
        Err(errors) => {
            for error in &errors {
                eprintln!("[{}] [config] invalid: {error}", protocol::ts());
            }
            eprintln!("[{}] [config] refusing to start; fix config.env", protocol::ts());
            None
        }
    }
}

fn open_db(settings: &Settings) -> Option<Db> {
    if let Some(parent) = std::path::Path::new(&settings.db_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    match db::open(&settings.db_path, settings.db_migration_backups_keep) {
        Ok(db) => Some(db),
        Err(e) => {
            let ts = protocol::ts();
            eprintln!("[{ts}] [db] ************************************************************");
            eprintln!("[{ts}] [db] REFUSING TO START: {e:#}");
            eprintln!("[{ts}] [db] ************************************************************");
            None
        }
    }
}

async fn serve(settings: Settings) -> ExitCode {
    DEBUG_LOGGING.store(settings.debug_logging, Ordering::Relaxed);
    for line in settings.summary() {
        log(format!("[config] {line}"));
    }
    for warning in settings.warnings() {
        log(format!("[config] warning: {warning}"));
    }

    let Some(db) = open_db(&settings) else {
        return ExitCode::FAILURE;
    };
    log(format!(
        "[db] {} is ready (this build's schema version is {})",
        settings.db_path,
        db::SCHEMA_VERSION
    ));
    match db.blocking(ReplyRunner::orphan_interrupted) {
        Ok(0) => {}
        Ok(n) => log(format!(
            "[reply] {n} reply(s) interrupted by the last shutdown marked orphaned"
        )),
        Err(e) => log(format!("[reply] error orphaning interrupted replies: {e:#}")),
    }

    let live = Arc::new(Live::new(settings.metrics));
    log(format!("[live] epoch {}", live.epoch));
    let auth = match Auth::new(&settings) {
        Ok(auth) => Arc::new(auth),
        Err(e) => {
            eprintln!("[{}] [auth] {e:#}", protocol::ts());
            return ExitCode::FAILURE;
        }
    };
    let settings = Arc::new(settings);
    let replies = ReplyRunner::new(db.clone(), live.clone(), settings.chat, settings.client.inference);
    let state = AppState {
        db,
        live,
        catalog: Arc::new(Catalog::from_settings(&settings)),
        settings,
        auth,
        replies,
        model_commands: Arc::new(ModelCommands::default()),
    };

    match state.db.call(|c| auth::store::admin_exists(c)).await {
        Ok(true) => {}
        Ok(false) => {
            let token = state.auth.issue_setup_token();
            let origin = &state.settings.http.public_origin;
            log("[setup] ================================================================");
            log("[setup] No admin account exists yet.");
            log(format!(
                "[setup] Open {origin}/setup and enter this setup token: {token}"
            ));
            log("[setup] It's valid until the admin is created or the server restarts.");
            log("[setup] ================================================================");
        }
        Err(e) => {
            eprintln!("[{}] [db] {e:#}", protocol::ts());
            return ExitCode::FAILURE;
        }
    }

    tokio::spawn(flush_loop(state.clone()));
    tokio::spawn(cleanup_loop(state.clone()));

    // The built React dashboard (`npm run build` in frontend/). Default is
    // relative so it resolves both from the repo root natively and from
    // /app in the Docker image.
    let frontend_dir = &state.settings.frontend_dir;
    if !std::path::Path::new(frontend_dir).join("index.html").is_file() {
        log(format!(
            "[dashboard] no index.html in {frontend_dir}, dashboard will 404 (build frontend/ first)"
        ));
    }

    let app = routes::app(state.clone());
    let port = state.settings.port;
    let addr = format!("0.0.0.0:{port}");
    log(format!("delegation-server listening on :{port}"));
    log(format!("  enroll:    POST http://localhost:{port}/enroll"));
    log(format!("  ws:        ws://localhost:{port}/ws"));
    log(format!("  dashboard: http://localhost:{port}/ (from {frontend_dir})"));

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("[{}] couldn't listen on {addr}: {e}", protocol::ts());
            return ExitCode::FAILURE;
        }
    };
    // Upgraded WebSockets and reply streams run on their own tasks, so this
    // returns once in-flight HTTP requests finish without waiting for them.
    let served = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await;
    if let Err(e) = served {
        log(format!("[shutdown] server error: {e}"));
    }

    flush_last_seen(&state, None).await;
    log("[shutdown] last_seen_at flushed, exiting");
    ExitCode::SUCCESS
}

/// `docker stop` sends SIGTERM; Ctrl-C covers native runs.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    log("[shutdown] signal received, stopping");
}

// ---------------- background tasks ----------------

/// Writes changed `last_seen_at` values every `LAST_SEEN_FLUSH_INTERVAL_MS`.
async fn flush_loop(state: AppState) {
    let mut interval = tokio::time::interval(state.settings.last_seen_flush_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval.tick().await; // the first tick is immediate
    loop {
        interval.tick().await;
        flush_last_seen(&state, None).await;
    }
}

/// Writes changed `last_seen_at` values (all, or just `only`'s) to SQLite
/// in one transaction. A client whose row has gone was deleted while
/// connected, so its connection is closed.
pub async fn flush_last_seen(state: &AppState, only: Option<&str>) {
    let rows = state.live.take_dirty(only);
    if rows.is_empty() {
        return;
    }
    let to_write = rows.clone();
    match state.db.call(move |c| db::write_last_seen(c, &to_write)).await {
        Ok(missing) => {
            debug(|| format!("[last_seen] flushed {} row(s)", rows.len()));
            for client_id in missing {
                log(format!("[ws] client {client_id} no longer exists, closing"));
                state.live.forget(&client_id);
            }
        }
        Err(e) => {
            log(format!("[last_seen] flush failed, will retry: {e:#}"));
            state.live.mark_dirty(&rows);
        }
    }
}

const CLEANUP_INTERVAL: Duration = Duration::from_secs(600);
const RETENTION_INTERVAL: Duration = Duration::from_secs(86_400);

/// Expired sessions and reset links, old pairing codes and rate-limit
/// entries every few minutes; old conversations daily when
/// `CHAT_RETENTION_DAYS` is set.
async fn cleanup_loop(state: AppState) {
    let mut cleanup = tokio::time::interval(CLEANUP_INTERVAL);
    cleanup.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut retention = tokio::time::interval(RETENTION_INTERVAL);
    retention.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cleanup.tick() => {
                let idle = state.settings.auth.session_idle_timeout;
                let result = state
                    .db
                    .call(move |c| {
                        let now = SystemTime::now();
                        let (sessions, resets) = auth::store::delete_expired(c, now, now - idle)?;
                        let codes = db::delete_old_pairing_codes(c)?;
                        Ok((sessions, resets, codes))
                    })
                    .await;
                match result {
                    Ok((s, r, p)) => debug(|| format!("[cleanup] removed {s} session(s), {r} reset link(s), {p} pairing code(s)")),
                    Err(e) => log(format!("[cleanup] error: {e:#}")),
                }
                state.auth.prune();
            }
            _ = retention.tick() => {
                let days = state.settings.chat.retention_days;
                if days == 0 {
                    continue;
                }
                let cutoff = db::ts(SystemTime::now() - Duration::from_secs(days.saturating_mul(86_400)));
                match state.db.call(move |c| conversations::delete_older_than(c, &cutoff)).await {
                    Ok(0) => {}
                    Ok(n) => log(format!("[retention] deleted {n} conversation(s) not updated in {days} day(s)")),
                    Err(e) => log(format!("[retention] error: {e:#}")),
                }
            }
        }
    }
}

// ---------------- CLI subcommands ----------------

/// `delegation-server reset-admin-password`, e.g. through
/// `docker compose exec delegation-server delegation-server reset-admin-password`.
fn reset_admin_password(settings: &Settings) -> ExitCode {
    let Some(db) = open_db(settings) else {
        return ExitCode::FAILURE;
    };
    let admin = match db.blocking(|c| {
        let Some(id) = auth::store::admin_id(c)? else {
            return Ok(None);
        };
        auth::store::find_by_id(c, &id)
    }) {
        Ok(Some(admin)) => admin.user,
        Ok(None) => {
            eprintln!(
                "No admin account exists yet. Start the server and create one at /setup with the token from its log."
            );
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let password = match read_new_password(&admin.username) {
        Ok(password) => password,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = auth::validate_password(settings, &password) {
        eprintln!("error: {}", e.message);
        return ExitCode::FAILURE;
    }
    let hash = match auth::password::Hasher::new(settings.auth.argon2).and_then(|h| h.hash_blocking(&password)) {
        Ok(hash) => hash,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = db.blocking(|c| auth::store::set_password(c, &admin.id, &hash, None)) {
        eprintln!("error: {e:#}");
        return ExitCode::FAILURE;
    }
    println!(
        "Password updated for {}. Every one of their sessions was signed out \
         (a running server stops accepting them within a few seconds).",
        admin.username
    );
    ExitCode::SUCCESS
}

/// Prompts twice on a terminal; otherwise reads one line from stdin.
fn read_new_password(username: &str) -> Result<String, String> {
    if std::io::stdin().is_terminal() {
        let first = rpassword::prompt_password(format!("New password for {username}: ")).map_err(|e| e.to_string())?;
        let second = rpassword::prompt_password("Again: ").map_err(|e| e.to_string())?;
        if first != second {
            return Err("the passwords don't match".into());
        }
        Ok(first)
    } else {
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
}

/// `delegation-server backup [FILE]`: `VACUUM INTO`, safe while the server
/// is running.
fn backup(settings: &Settings, dest: Option<&str>) -> ExitCode {
    let Some(db) = open_db(settings) else {
        return ExitCode::FAILURE;
    };
    let dest = dest.map(str::to_string).unwrap_or_else(|| {
        let dir = std::path::Path::new(&settings.db_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let stamp = protocol::ts().replace(':', "");
        dir.join(format!("delegation-backup-{stamp}.db"))
            .to_string_lossy()
            .into_owned()
    });
    match db.blocking(|c| db::backup(c, &dest)) {
        Ok(()) => {
            let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            println!("Backup written to {dest} ({size} bytes)");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
