//! The connection to one server: handshake, heartbeats, reconnect with
//! backoff. Front ends drive it through `controller`, sending commands in
//! and getting status updates back, so the CLI and the tray share exactly
//! the same connection logic.

use crate::hardware::MachineInfo;
use crate::identity::Identity;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::Signer;
use futures_util::{SinkExt, StreamExt};
use log::{info, warn};
use protocol::{config::Config, AuthFailedReason, ClientMessage, ServerMessage};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

/// The CLIENT_*_MS values from config.env.
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    /// How long we'll tolerate going without a heartbeat_ack before treating
    /// the connection as dead and forcing a reconnect. This is what catches
    /// a silently dropped connection (dead wifi, out-of-range client) that
    /// never produces a transport-level error — see the hurdles writeup.
    pub ack_timeout: Duration,
    pub heartbeat_interval: Duration,
    pub reconnect_initial_backoff: Duration,
    pub reconnect_max_backoff: Duration,
}

impl Timings {
    pub fn from_config(config: &Config) -> Self {
        Timings {
            ack_timeout: config.ms("CLIENT_ACK_TIMEOUT_MS", 30_000),
            heartbeat_interval: config.ms("CLIENT_HEARTBEAT_INTERVAL_MS", 10_000),
            reconnect_initial_backoff: config.ms("CLIENT_RECONNECT_INITIAL_BACKOFF_MS", 1_000),
            reconnect_max_backoff: config.ms("CLIENT_RECONNECT_MAX_BACKOFF_MS", 30_000),
        }
    }
}

/// A server and the identity this machine holds for it.
#[derive(Clone)]
pub struct ServerIdentity {
    /// HTTP base URL, e.g. "https://llm.smith-c.com".
    pub url: String,
    pub identity: Identity,
}

/// What gets reported in the `info` message after each successful auth.
/// The hardware scan runs once in the background; until it finishes this
/// holds `None` and the session sends `info` as soon as it does.
#[derive(Clone)]
pub struct ClientInfo {
    pub machine: watch::Receiver<Option<Arc<MachineInfo>>>,
}

// Commands are rare, so the size of an inline identity doesn't matter.
#[allow(clippy::large_enum_variant)]
pub enum SessionCmd {
    /// Stop whatever is running and connect to this server instead.
    Connect(ServerIdentity),
    Disconnect,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    Connecting,
    Connected,
    Disconnected {
        error: Option<String>,
    },
    /// The server has no record of our client_id. Retrying can't succeed,
    /// so the session stops; the front end decides how to re-enroll.
    UnknownClient,
}

/// Returned (inside `anyhow::Error`) by a session the server rejected as
/// an unknown client, so the reconnect loop knows to stop.
#[derive(Debug)]
pub enum SessionError {
    UnknownClient,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::UnknownClient => f.write_str("server doesn't recognize this client_id"),
        }
    }
}

impl std::error::Error for SessionError {}

/// Called with the URL of the server the status is about, so a front end
/// can ignore a late update from a session it has already replaced.
type StatusFn = Arc<dyn Fn(&str, SessionStatus) + Send + Sync>;

/// How long a stopping session gets to send a close frame before it's
/// simply aborted.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// Runs at most one session at a time until `Shutdown` arrives or the
/// command channel closes. `Connect` replaces whatever session is running,
/// `Disconnect` ends it; either way the server sees the socket close.
pub async fn controller(
    mut cmds: mpsc::Receiver<SessionCmd>,
    status: impl Fn(&str, SessionStatus) + Send + Sync + 'static,
    timings: Timings,
    info: ClientInfo,
) {
    let status: StatusFn = Arc::new(status);
    let mut current: Option<RunningSession> = None;
    while let Some(cmd) = cmds.recv().await {
        let stopped = match current.take() {
            Some(session) => Some(session.stop().await),
            None => None,
        };
        match cmd {
            SessionCmd::Connect(server) => {
                let (stop_tx, stop_rx) = watch::channel(false);
                let url = server.url.clone();
                let task = tokio::spawn(reconnect_loop(server, timings, info.clone(), status.clone(), stop_rx));
                current = Some(RunningSession {
                    url,
                    stop: stop_tx,
                    task,
                });
            }
            SessionCmd::Disconnect => {
                if let Some(url) = stopped {
                    status(&url, SessionStatus::Disconnected { error: None });
                }
            }
            SessionCmd::Shutdown => return,
        }
    }
    if let Some(session) = current.take() {
        session.stop().await;
    }
}

struct RunningSession {
    url: String,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl RunningSession {
    /// Asks the session to close cleanly, then aborts it if it hasn't
    /// finished within `STOP_GRACE` (e.g. stuck mid-handshake). Returns
    /// the server URL it was connected to.
    async fn stop(self) -> String {
        let _ = self.stop.send(true);
        let abort = self.task.abort_handle();
        if tokio::time::timeout(STOP_GRACE, self.task).await.is_err() {
            abort.abort();
        }
        self.url
    }
}

/// Basic reconnect-with-backoff loop. This is the piece worth stress
/// testing hardest on a real home connection: kill the tunnel mid-session
/// and confirm this recovers cleanly rather than spinning or silently dying.
async fn reconnect_loop(
    server: ServerIdentity,
    timings: Timings,
    info: ClientInfo,
    status: StatusFn,
    mut stop: watch::Receiver<bool>,
) {
    let ws_url = ws_url_for(&server.url);
    let status = |s: SessionStatus| status(&server.url, s);
    let mut backoff = timings.reconnect_initial_backoff;
    loop {
        info!(target: "ws", "connecting to {ws_url} ...");
        status(SessionStatus::Connecting);
        let mut authenticated = false;
        let result = run_session(
            &ws_url,
            &server.identity,
            &timings,
            &info,
            &status,
            &mut stop,
            &mut authenticated,
        )
        .await;
        if *stop.borrow() {
            info!(target: "ws", "disconnected");
            return;
        }
        // A session that got as far as authenticating proves the server is
        // reachable again, so the next drop starts from a short backoff.
        if authenticated {
            backoff = timings.reconnect_initial_backoff;
        }
        match result {
            Err(e) if matches!(e.downcast_ref(), Some(SessionError::UnknownClient)) => {
                warn!(target: "ws", "server doesn't recognize client_id {}, giving up", server.identity.client_id);
                status(SessionStatus::UnknownClient);
                return;
            }
            Ok(()) => info!(target: "ws", "session ended cleanly, reconnecting in {backoff:?}"),
            Err(e) => {
                warn!(target: "ws", "session error: {e:#}. Reconnecting in {backoff:?}");
                status(SessionStatus::Disconnected {
                    error: Some(format!("{e:#}")),
                });
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => {
                info!(target: "ws", "disconnected");
                return;
            }
        }
        backoff = std::cmp::min(backoff * 2, timings.reconnect_max_backoff);
    }
}

pub fn ws_url_for(server_http_base: &str) -> String {
    let ws_base = if let Some(rest) = server_http_base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = server_http_base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        server_http_base.to_string()
    };
    format!("{}/ws", ws_base.trim_end_matches('/'))
}

async fn run_session(
    ws_url: &str,
    identity: &Identity,
    timings: &Timings,
    info: &ClientInfo,
    status: &impl Fn(SessionStatus),
    stop: &mut watch::Receiver<bool>,
    authenticated: &mut bool,
) -> Result<()> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .context("websocket connect failed")?;
    let (mut write, mut read) = ws_stream.split();

    send(
        &mut write,
        &ClientMessage::Hello {
            client_id: identity.client_id.clone(),
        },
    )
    .await?;

    // --- handshake: expect a challenge, then send a signed response ---
    // The server actually rejects an unrecognized client_id right here,
    // immediately after Hello — before ever issuing a challenge — so
    // this is the real place AuthFailed{unknown_client_id} arrives, not
    // only after a challenge-response as the earlier (buggy) version of
    // this assumed. Caught by testing against a server with a reset DB:
    // the first attempt at this fix silently never triggered, and the
    // client looped on "expected challenge, got AuthFailed" forever
    // instead of self-healing.
    let challenge_msg = next_message(&mut read).await?;
    let nonce = match challenge_msg {
        ServerMessage::Challenge { nonce } => nonce,
        ServerMessage::AuthFailed {
            reason: AuthFailedReason::UnknownClient,
        } => return Err(SessionError::UnknownClient.into()),
        other => bail!("expected challenge, got {other:?}"),
    };
    let nonce_bytes = STANDARD.decode(&nonce).context("decoding nonce")?;
    let signature = identity.signing_key.sign(&nonce_bytes);
    send(
        &mut write,
        &ClientMessage::ChallengeResponse {
            signature: STANDARD.encode(signature.to_bytes()),
        },
    )
    .await?;

    let auth_result = next_message(&mut read).await?;
    match auth_result {
        ServerMessage::AuthOk {} => info!(target: "ws", "authenticated"),
        ServerMessage::AuthFailed {
            reason: AuthFailedReason::UnknownClient,
        } => return Err(SessionError::UnknownClient.into()),
        ServerMessage::AuthFailed { reason } => bail!("server rejected auth: {reason}"),
        other => bail!("expected auth_ok, got {other:?}"),
    }
    *authenticated = true;
    status(SessionStatus::Connected);

    // --- authenticated: info, then the heartbeat loop ---
    let mut machine = info.machine.clone();
    let mut info_sent = false;
    // Clone out first: the borrow guard isn't Send and must not be held
    // across the await.
    let ready = machine.borrow_and_update().clone();
    if let Some(m) = ready {
        send(&mut write, &info_message(&m)).await?;
        info_sent = true;
    }

    // Baseline set right after auth, so a server that never acks even the
    // first heartbeat gets caught by the same timeout rather than waiting
    // indefinitely for a "last" ack that never came.
    let mut last_ack = Instant::now();

    let mut ticker = tokio::time::interval(timings.heartbeat_interval);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // Self-heal on a silently dead connection: if the transport
                // hasn't told us anything is wrong but we also haven't heard
                // an ack in a while, the server has likely already given up
                // on us (its own heartbeat-timeout sweep) — don't wait for
                // an OS-level TCP error that may never come.
                if last_ack.elapsed() > timings.ack_timeout {
                    bail!("no heartbeat_ack received in {:?}, treating connection as dead", last_ack.elapsed());
                }
                send(&mut write, &ClientMessage::Heartbeat { timestamp: now_ms() }).await?;
                info!(target: "heartbeat", "sent");
            }
            // The hardware scan finished after we authenticated.
            Ok(()) = machine.changed(), if !info_sent => {
                let ready = machine.borrow_and_update().clone();
                if let Some(m) = ready {
                    send(&mut write, &info_message(&m)).await?;
                    info_sent = true;
                }
            }
            _ = stop.changed() => {
                let _ = write.send(Message::Close(None)).await;
                return Ok(());
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ServerMessage>(&text) {
                            Ok(ServerMessage::HeartbeatAck {}) => {
                                last_ack = Instant::now();
                                info!(target: "heartbeat", "acked");
                            }
                            Ok(other) => info!(target: "ws", "unexpected message during heartbeat loop: {other:?}"),
                            Err(e) => info!(target: "ws", "couldn't parse server message: {e}"),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => bail!("server closed connection"),
                    Some(Err(e)) => bail!("websocket error: {e}"),
                    _ => {}
                }
            }
        }
    }
}

fn info_message(machine: &MachineInfo) -> ClientMessage {
    ClientMessage::Info {
        hostname: crate::hostname(),
        os: machine.os.clone(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        specs: machine.specs.clone(),
    }
}

async fn send(
    write: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    msg: &ClientMessage,
) -> Result<()> {
    let text = serde_json::to_string(msg)?;
    write
        .send(Message::Text(text.into()))
        .await
        .context("sending websocket message")
}

async fn next_message(
    read: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Result<ServerMessage> {
    match read.next().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str(&text).context("parsing server message"),
        Some(Ok(other)) => bail!("expected text message, got {other:?}"),
        Some(Err(e)) => bail!("websocket error: {e}"),
        None => bail!("connection closed before handshake completed"),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
