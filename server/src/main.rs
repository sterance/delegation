mod db;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use db::Db;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use protocol::{
    ClientMessage, EnrollError, EnrollRequest, EnrollResponse, ServerMessage,
    AUTH_FAILED_UNKNOWN_CLIENT,
};
use rand::Rng;
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tower_http::services::ServeDir;
use uuid::Uuid;

/// client_id -> last time we heard a heartbeat from it. Presence in this
/// map is exactly what "connected" means to the dashboard; it's live,
/// in-memory state, deliberately not persisted — restarting the server
/// correctly shows everyone as disconnected until they reconnect.
type Connections = Arc<Mutex<HashMap<String, Instant>>>;

#[derive(Clone)]
struct AppState {
    db: Db,
    connections: Connections,
}

const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

fn log(msg: impl AsRef<str>) {
    println!("[{}] {}", protocol::ts(), msg.as_ref());
}

#[tokio::main]
async fn main() {
    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "data/delegation.db".to_string());
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let db = db::open(&db_path).expect("failed to open database");

    let state = AppState {
        db,
        connections: Arc::new(Mutex::new(HashMap::new())),
    };

    // The built React dashboard (`npm run build` in frontend/). Default is
    // relative so it resolves both from the repo root natively and from
    // /app in the Docker image.
    let frontend_dir = std::env::var("FRONTEND_DIR").unwrap_or_else(|_| "frontend/dist".to_string());
    if !std::path::Path::new(&frontend_dir).join("index.html").is_file() {
        log(format!("[dashboard] no index.html in {frontend_dir}, dashboard will 404 (build frontend/ first)"));
    }

    let app = Router::new()
        .route("/enroll", post(enroll))
        .route("/admin/pairing-code", post(create_pairing_code))
        .route("/admin/pairing-code/:code/status", get(pairing_code_status))
        .route("/clients", get(list_clients))
        .route("/ws", get(ws_upgrade))
        // Fallback, not a route: only requests no route above matched ever
        // reach the static files, so the dashboard can't shadow the API.
        .fallback_service(ServeDir::new(&frontend_dir))
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "7070".to_string());
    let addr = format!("0.0.0.0:{port}");
    log(format!("delegation-server listening on :{port}"));
    log(format!("  enroll:    POST http://localhost:{port}/enroll"));
    log(format!("  ws:        ws://localhost:{port}/ws"));
    log(format!("  dashboard: http://localhost:{port}/ (from {frontend_dir})"));

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
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
            Json(EnrollError { error: "invalid or expired pairing code".into() }),
        )
            .into_response(),
        Err(e) => {
            log(format!("[enroll] error: {e:#}"));
            (StatusCode::INTERNAL_SERVER_ERROR, Json(EnrollError { error: "internal error".into() }))
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
    match db::create_pairing_code(&state.db, 600, &code) {
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
async fn pairing_code_status(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> impl IntoResponse {
    match db::pairing_code_status(&state.db, &code) {
        Ok(Some((used, Some(client_id)))) => {
            let claimed_by = db::get_client(&state.db, &client_id)
                .ok()
                .flatten()
                .map(|c| ClaimedClient { client_id: c.client_id, hostname: c.hostname });
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

#[derive(Serialize)]
struct ClientListEntry {
    client_id: String,
    hostname: String,
    enrolled_at: String,
    last_seen_at: Option<String>,
    connected: bool,
}

async fn list_clients(State(state): State<AppState>) -> impl IntoResponse {
    match db::list_clients(&state.db) {
        Ok(rows) => {
            let connections = state.connections.lock().unwrap();
            let entries: Vec<ClientListEntry> = rows
                .into_iter()
                .map(|r| ClientListEntry {
                    connected: connections.contains_key(&r.client_id),
                    client_id: r.client_id,
                    hostname: r.hostname,
                    enrolled_at: r.enrolled_at,
                    last_seen_at: r.last_seen_at,
                })
                .collect();
            Json(entries).into_response()
        }
        Err(e) => {
            log(format!("[clients] error: {e:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------- WebSocket: challenge-response + heartbeat ----------------

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn send(socket: &mut WebSocket, msg: &ServerMessage) -> bool {
    let Ok(text) = serde_json::to_string(msg) else { return false };
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
    let Some(ClientMessage::Hello { client_id }) = recv_within(&mut socket, HANDSHAKE_TIMEOUT).await else {
        log("[ws] handshake timed out or malformed hello, closing");
        return;
    };

    let Ok(Some(record)) = db::get_client(&state.db, &client_id) else {
        log(format!("[ws] unknown client_id {client_id}"));
        send(&mut socket, &ServerMessage::AuthFailed { reason: AUTH_FAILED_UNKNOWN_CLIENT.to_string() }).await;
        return;
    };

    let mut nonce = [0u8; 16];
    rand::thread_rng().fill(&mut nonce);
    if !send(&mut socket, &ServerMessage::Challenge { nonce: STANDARD.encode(nonce) }).await {
        return;
    }

    let Some(ClientMessage::ChallengeResponse { signature }) =
        recv_within(&mut socket, HANDSHAKE_TIMEOUT).await
    else {
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
        send(&mut socket, &ServerMessage::AuthFailed { reason: "signature verification failed".into() }).await;
        return;
    }

    log(format!("[ws] client {client_id} authenticated"));
    send(&mut socket, &ServerMessage::AuthOk {}).await;
    state.connections.lock().unwrap().insert(client_id.clone(), Instant::now());

    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    ticker.tick().await; // first tick is immediate; consume it so we don't false-positive on the opening tick

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let stale = state.connections.lock().unwrap()
                    .get(&client_id)
                    .map(|last| last.elapsed() > HEARTBEAT_TIMEOUT)
                    .unwrap_or(true);
                if stale {
                    log(format!("[heartbeat] client {client_id} timed out, closing"));
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Heartbeat { timestamp: _, hostname }) => {
                                state.connections.lock().unwrap().insert(client_id.clone(), Instant::now());
                                let _ = db::touch_last_seen(&state.db, &client_id);
                                log(format!("[heartbeat] {client_id} ({hostname})"));
                                if !send(&mut socket, &ServerMessage::HeartbeatAck {}).await {
                                    break;
                                }
                            }
                            _ => log(format!("[ws] unexpected message from {client_id}")),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }

    state.connections.lock().unwrap().remove(&client_id);
    log(format!("[ws] client {client_id} disconnected"));
}

fn random_code() -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..8)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}
