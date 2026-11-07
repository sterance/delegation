mod config;
mod db;
mod live;
mod metrics;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use config::{ConfigFile, Settings};
use db::Db;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use live::{ConnHandle, Live, Sample, SampleOutcome};
use protocol::{
    AuthFailedReason, ClientMessage, EnrollError, EnrollRequest, EnrollResponse, HardwareSpecs, ServerMessage, Tick,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::time::MissedTickBehavior;
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: Db,
    /// Connections, metric history and unflushed `last_seen_at` values.
    live: Arc<Live>,
    settings: Arc<Settings>,
}

static DEBUG_LOGGING: AtomicBool = AtomicBool::new(false);

fn log(msg: impl AsRef<str>) {
    println!("[{}] {}", protocol::ts(), msg.as_ref());
}

/// Per-tick and per-sample detail, shown with `SERVER_LOG_LEVEL=debug`.
/// Takes a closure so nothing is formatted when it's off.
fn debug(msg: impl FnOnce() -> String) {
    if DEBUG_LOGGING.load(Ordering::Relaxed) {
        println!("[{}] {}", protocol::ts(), msg());
    }
}

#[tokio::main]
async fn main() {
    let file = ConfigFile::load(&["config.env".into()]);
    match file.source() {
        Some(path) => log(format!("[config] loaded {}", path.display())),
        None => log("[config] no config file, using environment and defaults"),
    }
    for warning in config::replaced_keys_in_use(|key| file.get(key)) {
        log(format!("[config] warning: {warning}"));
    }
    let settings = match Settings::parse(|key| file.get(key)) {
        Ok(settings) => settings,
        Err(errors) => {
            for error in &errors {
                eprintln!("[{}] [config] invalid: {error}", protocol::ts());
            }
            eprintln!("[{}] [config] refusing to start; fix config.env", protocol::ts());
            std::process::exit(1);
        }
    };
    DEBUG_LOGGING.store(settings.debug_logging, Ordering::Relaxed);
    for line in settings.summary() {
        log(format!("[config] {line}"));
    }

    if let Some(parent) = std::path::Path::new(&settings.db_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let db = db::open(&settings.db_path).expect("failed to open database");

    let live = Arc::new(Live::new(settings.metrics));
    log(format!("[live] epoch {}", live.epoch));
    let settings = Arc::new(settings);
    let state = AppState { db, live, settings };

    tokio::spawn(flush_loop(state.clone()));

    // The built React dashboard (`npm run build` in frontend/). Default is
    // relative so it resolves both from the repo root natively and from
    // /app in the Docker image.
    let frontend_dir = state.settings.frontend_dir.clone();
    if !std::path::Path::new(&frontend_dir).join("index.html").is_file() {
        log(format!(
            "[dashboard] no index.html in {frontend_dir}, dashboard will 404 (build frontend/ first)"
        ));
    }

    // Dashboard-facing endpoints live under /api so they never collide with
    // the dashboard's own client-side routes (/clients, /chat, ...). Unknown
    // /api paths 404 here instead of falling through to index.html.
    let api = Router::new()
        .route("/admin/pairing-code", post(create_pairing_code))
        .route("/admin/pairing-code/:code/status", get(pairing_code_status))
        .route("/admin/dashboard-config", get(dashboard_config))
        .route("/clients", get(list_clients))
        .route("/clients/:client_id", patch(update_client).delete(delete_client))
        .fallback(|| async { StatusCode::NOT_FOUND });

    // Any path that isn't a real file gets index.html, so deep links and
    // reloads of client-side routes load the app, which then routes itself.
    let spa = ServeDir::new(&frontend_dir).fallback(ServeFile::new(
        std::path::Path::new(&frontend_dir).join("index.html"),
    ));

    let app = Router::new()
        // Used by the client binary, so these stay at the root.
        .route("/enroll", post(enroll))
        .route("/ws", get(ws_upgrade))
        .nest("/api", api)
        // Fallback, not a route: only requests no route above matched ever
        // reach the static files, so the dashboard can't shadow the API.
        .fallback_service(spa)
        .with_state(state.clone());

    let port = state.settings.port;
    let addr = format!("0.0.0.0:{port}");
    log(format!("delegation-server listening on :{port}"));
    log(format!("  enroll:    POST http://localhost:{port}/enroll"));
    log(format!("  ws:        ws://localhost:{port}/ws"));
    log(format!("  dashboard: http://localhost:{port}/ (from {frontend_dir})"));

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    // Upgraded WebSockets run on their own tasks, so this returns once
    // in-flight HTTP requests finish without waiting for clients.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    flush_last_seen(&state, None);
    log("[shutdown] last_seen_at flushed, exiting");
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

// ---------------- last_seen_at ----------------

/// Writes changed `last_seen_at` values every `LAST_SEEN_FLUSH_INTERVAL_MS`.
async fn flush_loop(state: AppState) {
    let mut interval = tokio::time::interval(state.settings.last_seen_flush_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval.tick().await; // the first tick is immediate
    loop {
        interval.tick().await;
        flush_last_seen(&state, None);
    }
}

/// Writes changed `last_seen_at` values (all, or just `only`'s) to SQLite
/// in one transaction. A client whose row has gone was deleted while
/// connected, so its connection is closed.
fn flush_last_seen(state: &AppState, only: Option<&str>) {
    let rows = state.live.take_dirty(only);
    if rows.is_empty() {
        return;
    }
    match db::write_last_seen(&state.db, &rows) {
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

// ---------------- enrollment ----------------

async fn enroll(State(state): State<AppState>, Json(req): Json<EnrollRequest>) -> impl IntoResponse {
    let client_id = Uuid::new_v4().to_string();
    match db::try_enroll(&state.db, &req.pairing_code, &client_id, &req.public_key, &req.hostname) {
        Ok(db::EnrollOutcome::Enrolled) => {
            log(format!("[enroll] new client {client_id} ({})", req.hostname));
            (StatusCode::OK, Json(EnrollResponse { client_id })).into_response()
        }
        Ok(db::EnrollOutcome::InvalidCode) => (
            StatusCode::FORBIDDEN,
            Json(EnrollError {
                error: "invalid or expired pairing code".into(),
            }),
        )
            .into_response(),
        Err(e) => {
            log(format!("[enroll] error: {e:#}"));
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(EnrollError {
                    error: "internal error".into(),
                }),
            )
                .into_response()
        }
    }
}

// ---------------- admin / dashboard-facing endpoints ----------------
// No auth at this layer yet, deliberately — Cloudflare Access sits in
// front of the whole origin per the agreed plan. Revisit if this server
// is ever reachable any other way.

#[derive(Serialize)]
struct PairingCodeResponse {
    code: String,
}

async fn create_pairing_code(State(state): State<AppState>) -> impl IntoResponse {
    let code = random_code();
    let ttl_secs = i64::try_from(state.settings.pairing_code_ttl_secs).unwrap_or(i64::MAX);
    match db::create_pairing_code(&state.db, ttl_secs, &code) {
        Ok(()) => {
            log(format!("[admin] pairing code generated: {code}"));
            Json(PairingCodeResponse { code }).into_response()
        }
        Err(e) => {
            log(format!("[admin] error generating pairing code: {e:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Serialize)]
struct PairingStatusResponse {
    used: bool,
    claimed_by: Option<ClaimedClient>,
}

#[derive(Serialize)]
struct ClaimedClient {
    client_id: String,
    hostname: String,
}

/// Polled by the "Add client" modal: once `claimed_by` is populated, the
/// modal swaps from showing the code to showing the new client's info.
async fn pairing_code_status(State(state): State<AppState>, Path(code): Path<String>) -> impl IntoResponse {
    match db::pairing_code_status(&state.db, &code) {
        Ok(Some((used, Some(client_id)))) => {
            let claimed_by = db::get_client(&state.db, &client_id)
                .ok()
                .flatten()
                .map(|c| ClaimedClient {
                    client_id: c.client_id,
                    hostname: c.hostname,
                });
            Json(PairingStatusResponse { used, claimed_by }).into_response()
        }
        Ok(Some((used, None))) => Json(PairingStatusResponse { used, claimed_by: None }).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            log(format!("[admin] error checking pairing code status: {e:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The dashboard's runtime settings (graph look, poll intervals), so
/// changing them needs a server restart rather than a frontend rebuild.
async fn dashboard_config(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.settings.dashboard.clone())
}

#[derive(Serialize)]
struct ClientsResponse {
    /// Random per server start; a change means `seq` numbering restarted.
    epoch: String,
    /// The newest sample seq across all clients; the next poll's `since`.
    latest_seq: u64,
    clients: Vec<ClientListEntry>,
}

#[derive(Serialize)]
struct ClientListEntry {
    client_id: String,
    hostname: String,
    /// What the dashboard displays: set by the user, or the hostname.
    friendly_name: String,
    enrolled_at: String,
    last_seen_at: Option<String>,
    connected: bool,
    /// The rest come from the client's `info` message, so they're null
    /// until the client has connected at least once.
    os: Option<String>,
    client_version: Option<String>,
    specs: Option<HardwareSpecs>,
    info_updated_at: Option<String>,
    /// Samples newer than `since`, oldest first. Empty when disconnected.
    samples: Vec<Sample>,
}

#[derive(Deserialize)]
struct ClientsQuery {
    since: Option<u64>,
}

async fn list_clients(State(state): State<AppState>, Query(query): Query<ClientsQuery>) -> impl IntoResponse {
    match db::list_clients(&state.db) {
        Ok(rows) => {
            let mut snapshot = state.live.snapshot(query.since.unwrap_or(0));
            let clients = rows
                .into_iter()
                .map(|r| {
                    let samples = snapshot.connected.remove(&r.client_id);
                    ClientListEntry {
                        connected: samples.is_some(),
                        samples: samples.unwrap_or_default(),
                        last_seen_at: snapshot.last_seen.remove(&r.client_id).or(r.last_seen_at),
                        client_id: r.client_id,
                        hostname: r.hostname,
                        friendly_name: r.friendly_name,
                        enrolled_at: r.enrolled_at,
                        os: r.os,
                        client_version: r.client_version,
                        specs: r.specs_json.and_then(|json| serde_json::from_str(&json).ok()),
                        info_updated_at: r.info_updated_at,
                    }
                })
                .collect();
            Json(ClientsResponse {
                epoch: state.live.epoch.clone(),
                latest_seq: snapshot.latest_seq,
                clients,
            })
            .into_response()
        }
        Err(e) => {
            log(format!("[clients] error: {e:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

const MAX_FRIENDLY_NAME_CHARS: usize = 64;

#[derive(Deserialize)]
struct UpdateClientRequest {
    /// Blank or null resets to the hostname.
    friendly_name: Option<String>,
}

async fn update_client(
    State(state): State<AppState>,
    Path(client_id): Path<String>,
    Json(req): Json<UpdateClientRequest>,
) -> impl IntoResponse {
    let name = req.friendly_name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    if name.is_some_and(|n| n.chars().count() > MAX_FRIENDLY_NAME_CHARS) {
        return (
            StatusCode::BAD_REQUEST,
            format!("friendly_name is longer than {MAX_FRIENDLY_NAME_CHARS} characters"),
        )
            .into_response();
    }
    match db::set_friendly_name(&state.db, &client_id, name) {
        Ok(true) => {
            log(format!("[admin] {client_id} friendly name set to {name:?}"));
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            log(format!("[admin] error renaming {client_id}: {e:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Removes the client from the database and closes its socket if it's
/// connected. It then reconnects, gets `unknown_client`, and clears its
/// own identity.
async fn delete_client(State(state): State<AppState>, Path(client_id): Path<String>) -> impl IntoResponse {
    match db::delete_client(&state.db, &client_id) {
        Ok(true) => {
            state.live.forget(&client_id);
            log(format!("[admin] client {client_id} deleted"));
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            log(format!("[admin] error deleting {client_id}: {e:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------- WebSocket: challenge-response, then ticks ----------------

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn send(socket: &mut WebSocket, msg: &ServerMessage) -> bool {
    let Ok(text) = serde_json::to_string(msg) else {
        return false;
    };
    socket.send(Message::Text(text)).await.is_ok()
}

/// Waits for the next text message and parses it, with a timeout — used
/// only during the handshake, before we're tracking the connection as
/// authenticated at all.
async fn recv_within(socket: &mut WebSocket, timeout: Duration) -> Option<ClientMessage> {
    let msg = tokio::time::timeout(timeout, socket.recv()).await.ok()??;
    let Message::Text(text) = msg.ok()? else { return None };
    serde_json::from_str(&text).ok()
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let auth_timeout = state.settings.auth_timeout;
    let Some(ClientMessage::Hello { client_id }) = recv_within(&mut socket, auth_timeout).await else {
        log("[ws] handshake timed out or malformed hello, closing");
        return;
    };

    let Ok(Some(record)) = db::get_client(&state.db, &client_id) else {
        log(format!("[ws] unknown client_id {client_id}"));
        send(
            &mut socket,
            &ServerMessage::AuthFailed {
                reason: AuthFailedReason::UnknownClient,
            },
        )
        .await;
        return;
    };

    let mut nonce = [0u8; 16];
    rand::thread_rng().fill(&mut nonce);
    if !send(
        &mut socket,
        &ServerMessage::Challenge {
            nonce: STANDARD.encode(nonce),
        },
    )
    .await
    {
        return;
    }

    let Some(ClientMessage::ChallengeResponse { signature }) = recv_within(&mut socket, auth_timeout).await else {
        log(format!("[ws] {client_id} didn't respond to challenge in time"));
        return;
    };

    let valid = (|| -> Option<bool> {
        let pubkey_bytes: [u8; 32] = STANDARD.decode(&record.public_key).ok()?.try_into().ok()?;
        let verifying_key = VerifyingKey::from_bytes(&pubkey_bytes).ok()?;
        let sig_bytes: [u8; 64] = STANDARD.decode(&signature).ok()?.try_into().ok()?;
        let sig = Signature::from_bytes(&sig_bytes);
        Some(verifying_key.verify(&nonce, &sig).is_ok())
    })()
    .unwrap_or(false);

    if !valid {
        log(format!("[ws] bad signature from {client_id}"));
        send(
            &mut socket,
            &ServerMessage::AuthFailed {
                reason: AuthFailedReason::BadSignature,
            },
        )
        .await;
        return;
    }

    if !send(
        &mut socket,
        &ServerMessage::Welcome {
            settings: state.settings.client,
        },
    )
    .await
    {
        return;
    }
    log(format!("[ws] client {client_id} authenticated"));
    let conn = state.live.register(&client_id);
    let mut session = Session {
        state: &state,
        client_id: &client_id,
        hostname: record.hostname,
        conn: &conn,
        next_seq: 0,
    };
    let reason = session.run(&mut socket).await;

    state.live.unregister(&client_id, conn.id);
    flush_last_seen(&state, Some(&client_id));
    log(format!("[ws] client {client_id} disconnected: {reason}"));
}

/// One authenticated connection.
struct Session<'a> {
    state: &'a AppState,
    client_id: &'a str,
    /// For log lines only; updated when the client reports a new name.
    hostname: String,
    conn: &'a ConnHandle,
    /// The tick seq we expect next, to log drops at debug level.
    next_seq: u64,
}

impl Session<'_> {
    /// Runs until the connection should end; returns why, for the log.
    async fn run(&mut self, socket: &mut WebSocket) -> String {
        let settings = &self.state.settings;
        let liveness_timeout = settings.liveness_timeout;
        let mut last_inbound = Instant::now();

        // Checks liveness at least once per tick interval.
        let mut watchdog = tokio::time::interval(settings.client.tick_interval());
        watchdog.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Pings let the client tell a dead server from a quiet one; its
        // WebSocket library answers them with pongs automatically.
        let mut ping = tokio::time::interval(settings.ws_ping_interval);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ping.tick().await; // the first tick is immediate

        loop {
            tokio::select! {
                _ = self.conn.kick.notified() => {
                    let _ = tokio::time::timeout(Duration::from_secs(1), socket.send(Message::Close(None))).await;
                    return "closed by the server (deleted, or replaced by a newer connection)".into();
                }
                _ = watchdog.tick() => {
                    let silent = last_inbound.elapsed();
                    if silent >= liveness_timeout {
                        return format!("no frame for {} ms, timed out", silent.as_millis());
                    }
                }
                _ = ping.tick() => {
                    // Bounded, so a peer that stopped reading can't wedge
                    // this loop (and with it the watchdog).
                    match tokio::time::timeout(liveness_timeout, socket.send(Message::Ping(Vec::new()))).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => return format!("ping failed: {e}"),
                        Err(_) => return "ping send timed out".into(),
                    }
                }
                msg = socket.recv() => {
                    let msg = match msg {
                        Some(Ok(msg)) => msg,
                        Some(Err(e)) => return format!("websocket error: {e}"),
                        None => return "connection closed".into(),
                    };
                    // Any frame at all is proof of life: ticks, pongs, even
                    // ones that fail to parse.
                    last_inbound = Instant::now();
                    self.state.live.touch(self.client_id);
                    match msg {
                        Message::Text(text) => self.on_text(&text),
                        Message::Close(_) => return "client closed the connection".into(),
                        // Pongs only matter for liveness; pings are answered
                        // by axum.
                        Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => {}
                    }
                }
            }
        }
    }

    fn on_text(&mut self, text: &str) {
        let client_id = self.client_id;
        match serde_json::from_str::<ClientMessage>(text) {
            Ok(ClientMessage::Tick(tick)) => self.on_tick(tick),
            Ok(ClientMessage::Info {
                hostname,
                os,
                client_version,
                specs,
            }) => {
                let specs_json = serde_json::to_string(&specs).unwrap_or_else(|_| "{}".into());
                match db::update_client_info(&self.state.db, client_id, &hostname, &os, &client_version, &specs_json) {
                    Ok(()) => log(format!(
                        "[info] {client_id} ({hostname}): {os}, client {client_version}"
                    )),
                    Err(e) => log(format!("[info] error saving info for {client_id}: {e:#}")),
                }
                self.hostname = hostname;
            }
            Ok(ClientMessage::Hello { .. } | ClientMessage::ChallengeResponse { .. }) => {
                log(format!("[ws] unexpected handshake message from {client_id}"))
            }
            Err(e) => debug(|| format!("[ws] unparseable message from {client_id}: {e}")),
        }
    }

    fn on_tick(&mut self, Tick { seq, metrics }: Tick) {
        let (client_id, hostname) = (self.client_id, &self.hostname);
        if seq != self.next_seq {
            debug(|| {
                format!(
                    "[tick] {client_id} ({hostname}): expected seq {}, got {seq}",
                    self.next_seq
                )
            });
        }
        self.next_seq = seq.wrapping_add(1);
        let Some(metrics) = metrics else {
            debug(|| format!("[tick] {client_id} ({hostname}) seq {seq}: no metrics"));
            return;
        };
        let age = metrics.sample_age_ms;
        match self
            .state
            .live
            .record_sample(client_id, self.conn.id, metrics, Instant::now())
        {
            SampleOutcome::Accepted { seq: sample_seq } => {
                debug(|| format!("[tick] {client_id} ({hostname}) seq {seq}: sample {sample_seq}, {age} ms old"))
            }
            SampleOutcome::Rejected(reason) => {
                debug(|| format!("[tick] {client_id} ({hostname}) seq {seq}: sample dropped: {reason}"))
            }
            SampleOutcome::NotConnected => {}
        }
    }
}

fn random_code() -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..8).map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char).collect()
}
