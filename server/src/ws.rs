//! `/ws`: a client's WebSocket. Challenge-response with its Ed25519 key,
//! then ticks (metrics and liveness) in, chat requests out, and chat
//! replies in, handed to the reply runner. Outside session auth: clients
//! authenticate with their keys.

use crate::live::{ConnHandle, SampleOutcome};
use crate::replies::ChatEvent;
use crate::{db, debug, flush_last_seen, log, AppState};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use protocol::{valid_model_id, AuthFailedReason, ClientMessage, Inventory, ServerMessage, Tick};
use rand::Rng;
use std::time::{Duration, Instant};
use tokio::time::MissedTickBehavior;

pub async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn send(socket: &mut WebSocket, msg: &ServerMessage) -> bool {
    let Ok(text) = serde_json::to_string(msg) else {
        return false;
    };
    socket.send(Message::Text(text)).await.is_ok()
}

/// Waits for the next text message and parses it, with a timeout. Only
/// used during the handshake.
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

    let id = client_id.clone();
    let record = match state.db.call(move |c| db::get_client(c, &id)).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            log(format!("[ws] unknown client_id {client_id}"));
            send(
                &mut socket,
                &ServerMessage::AuthFailed {
                    reason: AuthFailedReason::UnknownClient,
                },
            )
            .await;
            return;
        }
        Err(e) => {
            log(format!("[ws] error looking up {client_id}: {e:#}"));
            return;
        }
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
    log(format!(
        "[ws] client {client_id} authenticated (owner {})",
        record.owner_user_id
    ));
    let mut conn = state.live.register(&client_id);
    let conn_id = conn.id;
    let mut session = Session {
        state: &state,
        client_id: &client_id,
        hostname: record.hostname,
        conn: &mut conn,
        next_seq: 0,
        last_inventory: None,
    };
    let reason = session.run(&mut socket).await;

    state.live.unregister(&client_id, conn_id);
    state.replies.on_connection_closed(conn_id);
    state.model_commands.on_connection_closed(conn_id);
    flush_last_seen(&state, Some(&client_id)).await;
    log(format!("[ws] client {client_id} disconnected: {reason}"));
}

/// One authenticated connection.
struct Session<'a> {
    state: &'a AppState,
    client_id: &'a str,
    /// For log lines only; updated when the client reports a new name.
    hostname: String,
    conn: &'a mut ConnHandle,
    /// The tick seq we expect next, to log drops at debug level.
    next_seq: u64,
    /// The last inventory logged, so an unchanged one isn't logged again.
    last_inventory: Option<String>,
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
                // Chat requests and cancels queued by the reply runner.
                Some(out) = self.conn.outbound.recv() => {
                    let Ok(text) = serde_json::to_string(&out) else { continue };
                    match tokio::time::timeout(liveness_timeout, socket.send(Message::Text(text))).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => return format!("send failed: {e}"),
                        Err(_) => return "send timed out".into(),
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
                log(format!(
                    "[info] {client_id} ({hostname}): {os}, client {client_version}"
                ));
                self.hostname = hostname.clone();
                let (db, id) = (self.state.db.clone(), client_id.to_string());
                tokio::spawn(async move {
                    let result = db
                        .call({
                            let id = id.clone();
                            move |c| db::update_client_info(c, &id, &hostname, &os, &client_version, &specs_json)
                        })
                        .await;
                    if let Err(e) = result {
                        log(format!("[info] error saving info for {id}: {e:#}"));
                    }
                });
            }
            Ok(ClientMessage::ModelInventory(inventory)) => match check_inventory(&inventory) {
                Ok(()) => {
                    let summary: Vec<String> = inventory
                        .models
                        .iter()
                        .map(|m| format!("{} ({:?})", m.id, m.state).to_lowercase())
                        .collect();
                    let summary = summary.join(", ");
                    if self.last_inventory.as_deref() != Some(summary.as_str()) {
                        log(format!("[models] {client_id} ({}): {summary}", self.hostname));
                        self.last_inventory = Some(summary);
                    }
                    self.state.live.set_inventory(client_id, self.conn.id, inventory);
                }
                Err(why) => log(format!("[models] ignored an inventory from {client_id}: {why}")),
            },
            Ok(ClientMessage::ModelCommandStatus {
                command_id,
                state,
                progress,
                error,
            }) => {
                self.state
                    .model_commands
                    .on_status(self.conn.id, &command_id, state, progress, error);
            }
            Ok(ClientMessage::ChatProgress { request_id, stage }) => {
                debug(|| format!("[chat] {request_id}: {stage:?}"));
                self.state
                    .replies
                    .on_client_event(self.conn.id, &request_id, ChatEvent::Progress(stage));
            }
            Ok(ClientMessage::ChatChunk { request_id, delta }) => {
                self.state
                    .replies
                    .on_client_event(self.conn.id, &request_id, ChatEvent::Chunk(delta));
            }
            Ok(ClientMessage::ChatDone { request_id }) => {
                debug(|| format!("[chat] {request_id} done"));
                self.state
                    .replies
                    .on_client_event(self.conn.id, &request_id, ChatEvent::Done);
            }
            Ok(ClientMessage::ChatError { request_id, error }) => {
                log(format!("[chat] {request_id} failed on {client_id}: {error}"));
                self.state
                    .replies
                    .on_client_event(self.conn.id, &request_id, ChatEvent::Error(error));
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

/// Limits on what a client may report, so one can't make the server hold
/// (and every dashboard poll copy) something huge.
const MAX_INVENTORY_MODELS: usize = 256;
const MAX_INVENTORY_STRING: usize = 512;

fn check_inventory(inventory: &Inventory) -> Result<(), String> {
    if inventory.models.len() > MAX_INVENTORY_MODELS || inventory.runtimes.len() > 32 {
        return Err("too many entries".into());
    }
    let long = |s: &Option<String>| s.as_ref().is_some_and(|s| s.len() > MAX_INVENTORY_STRING);
    for m in &inventory.models {
        if !valid_model_id(&m.id) {
            return Err(format!("invalid model id {:?}", m.id));
        }
        if m.display_name.len() > MAX_INVENTORY_STRING
            || long(&m.meta.family)
            || long(&m.meta.parameters)
            || long(&m.meta.quantization)
            || m.error.as_ref().is_some_and(|e| e.len() > 8 * MAX_INVENTORY_STRING)
        {
            return Err(format!("a string about {} is too long", m.id));
        }
    }
    if inventory
        .runtimes
        .iter()
        .any(|r| r.version.len() > 64 || r.variant.len() > 64)
    {
        return Err("a runtime string is too long".into());
    }
    Ok(())
}
