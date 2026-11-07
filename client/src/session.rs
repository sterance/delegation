//! The connection to one server: handshake, ticks, server-silence
//! detection, reconnect with backoff. Front ends drive it through
//! `controller`, sending commands in and getting status updates back, so
//! the CLI and the tray share exactly the same connection logic.

use crate::chat;
use crate::hardware::MachineInfo;
use crate::identity::Identity;
use crate::sampler::Sampler;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::Signer;
use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use protocol::{AuthFailedReason, ClientMessage, ClientSettings, ServerMessage, Tick};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;

/// A server and the identity this machine holds for it.
#[derive(Clone)]
pub struct ServerIdentity {
    /// HTTP base URL, e.g. "https://llm.smith-c.com".
    pub url: String,
    pub identity: Identity,
    /// The settings this server last sent in `welcome` (the front end
    /// persists them), or the defaults if it never has. Only the reconnect
    /// backoff and the first connection's timeouts use these; every
    /// `welcome` replaces them for the rest of the session.
    pub settings: ClientSettings,
}

/// What gets reported to the server. The hardware scan runs once in the
/// background; until it finishes `machine` holds `None` and the session
/// sends `info` as soon as it does. The sampler runs for the life of the
/// process, feeding every tick.
#[derive(Clone)]
pub struct ClientInfo {
    pub machine: watch::Receiver<Option<Arc<MachineInfo>>>,
    pub sampler: Sampler,
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
    /// Authenticated. Carries the settings from `welcome`, for the front end
    /// to persist next to this server's saved entry.
    Connected(ClientSettings),
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
                let task = tokio::spawn(reconnect_loop(server, info.clone(), status.clone(), stop_rx));
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
async fn reconnect_loop(server: ServerIdentity, info: ClientInfo, status: StatusFn, mut stop: watch::Receiver<bool>) {
    let ws_url = ws_url_for(&server.url);
    let status = |s: SessionStatus| status(&server.url, s);
    let mut settings = server.settings.sanitized();
    let mut backoff = settings.reconnect_backoff_initial();
    loop {
        info!(target: "ws", "connecting to {ws_url} ...");
        status(SessionStatus::Connecting);
        let mut authenticated = false;
        let result = run_session(
            &ws_url,
            &server.identity,
            &info,
            &status,
            &mut stop,
            &mut settings,
            &mut authenticated,
        )
        .await;
        if *stop.borrow() {
            info!(target: "ws", "disconnected");
            return;
        }
        // A session that got as far as authenticating proves the server is
        // reachable again, so the next drop starts from a short backoff
        // (and from the settings it just sent).
        if authenticated {
            backoff = settings.reconnect_backoff_initial();
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
        backoff = std::cmp::min(backoff * 2, settings.reconnect_backoff_max());
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
    info: &ClientInfo,
    status: &impl Fn(SessionStatus),
    stop: &mut watch::Receiver<bool>,
    settings: &mut ClientSettings,
    authenticated: &mut bool,
) -> Result<()> {
    // Until `welcome`, the cached silence timeout bounds each step, so a
    // dead network can't leave the connect or handshake hanging on TCP's
    // own (minutes-long) timeouts.
    let silence = settings.server_silence_timeout();
    let (ws_stream, _) = within(silence, "websocket connect", tokio_tungstenite::connect_async(ws_url))
        .await?
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
    let challenge_msg = within(silence, "challenge", next_message(&mut read)).await??;
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

    let auth_result = within(silence, "welcome", next_message(&mut read)).await??;
    match auth_result {
        ServerMessage::Welcome { settings: pushed } => {
            *settings = pushed.sanitized();
            info!(target: "ws", "authenticated");
            info!(
                target: "ws",
                "server settings: tick {} ms, sampler {} ms, sample max age {} ms, silence timeout {} ms, \
                 backoff {}..{} ms",
                settings.tick_interval_ms,
                settings.sampler_interval_ms,
                settings.sample_max_age_ms,
                settings.server_silence_timeout_ms,
                settings.reconnect_backoff_initial_ms,
                settings.reconnect_backoff_max_ms
            );
        }
        ServerMessage::AuthFailed {
            reason: AuthFailedReason::UnknownClient,
        } => return Err(SessionError::UnknownClient.into()),
        ServerMessage::AuthFailed { reason } => bail!("server rejected auth: {reason}"),
        other => bail!("expected welcome, got {other:?}"),
    }
    *authenticated = true;
    let settings = *settings;
    info.sampler.set_interval(settings.sampler_interval());
    status(SessionStatus::Connected(settings));

    // --- authenticated: info, then ticks until the connection ends ---
    let mut machine = info.machine.clone();
    let mut info_sent = false;
    // Clone out first: the borrow guard isn't Send and must not be held
    // across the await.
    let ready = machine.borrow_and_update().clone();
    if let Some(m) = ready {
        send(&mut write, &info_message(&m)).await?;
        info_sent = true;
    }
    send(&mut write, &ClientMessage::Models { models: chat::models() }).await?;

    // Chat replies are generated on their own tasks and queued here for
    // this loop to send. Dropping `chats` (however the session ends) aborts
    // any still running, so nothing keeps generating for a dead socket.
    let (chat_tx, mut chat_rx) = mpsc::channel::<ClientMessage>(64);
    let mut chats = ChatTasks::default();

    // Scheduled from a fixed start, so ticks don't drift; after a stall
    // (a suspended laptop, a blocked send) the missed ones are skipped
    // rather than sent as a burst. The first tick is immediate.
    let mut ticker = tokio::time::interval(settings.tick_interval());
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut seq: u64 = 0;

    // Any frame from the server counts, including its pings. This is what
    // catches a silently dropped connection (dead Wi-Fi, a server that's
    // gone) that never produces a transport-level error.
    let silence = settings.server_silence_timeout();
    let mut last_inbound = Instant::now();

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // Never waits on the sampler: this copies its latest reading.
                let metrics = info.sampler.metrics_for_tick(settings.sample_max_age());
                let has_metrics = metrics.is_some();
                within(silence, "sending a tick", send(&mut write, &ClientMessage::Tick(Tick { seq, metrics }))).await??;
                debug!(target: "tick", "sent {seq} (metrics: {has_metrics})");
                seq += 1;
            }
            Some(out) = chat_rx.recv() => {
                within(silence, "sending a chat reply", send(&mut write, &out)).await??;
            }
            _ = tokio::time::sleep_until(last_inbound + silence) => {
                bail!("nothing from the server in {} ms, treating the connection as dead", silence.as_millis());
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
            // Reading continuously also keeps pongs flowing: tungstenite
            // queues a pong for every ping it reads and writes it out on the
            // next read or write.
            msg = read.next() => {
                let msg = match msg {
                    Some(Ok(msg)) => msg,
                    Some(Err(e)) => bail!("websocket error: {e}"),
                    None => bail!("server closed connection"),
                };
                last_inbound = Instant::now();
                match msg {
                    Message::Text(text) => match serde_json::from_str::<ServerMessage>(&text) {
                        Ok(ServerMessage::ChatRequest { request_id, model, messages }) => {
                            info!(target: "chat", "request {request_id}: {model}, {} message(s)", messages.len());
                            let task = tokio::spawn(chat::respond(request_id.clone(), model, messages, chat_tx.clone()));
                            chats.start(request_id, task);
                        }
                        Ok(ServerMessage::ChatCancel { request_id }) => {
                            if chats.cancel(&request_id) {
                                info!(target: "chat", "request {request_id} cancelled");
                            }
                        }
                        Ok(other) => info!(target: "ws", "unexpected message after welcome: {other:?}"),
                        Err(e) => info!(target: "ws", "couldn't parse server message: {e}"),
                    },
                    Message::Close(_) => bail!("server closed connection"),
                    _ => {}
                }
            }
        }
    }
}

/// The chat replies being generated, by request_id. Aborts them all when
/// dropped.
#[derive(Default)]
struct ChatTasks(HashMap<String, tokio::task::AbortHandle>);

impl ChatTasks {
    fn start(&mut self, request_id: String, task: tokio::task::JoinHandle<()>) {
        // Finished ones are only pruned here, so the map stays as small as
        // the number of replies in progress.
        self.0.retain(|_, handle| !handle.is_finished());
        self.0.insert(request_id, task.abort_handle());
    }

    /// Stops a reply; returns whether it was still running.
    fn cancel(&mut self, request_id: &str) -> bool {
        let Some(handle) = self.0.remove(request_id) else {
            return false;
        };
        let running = !handle.is_finished();
        handle.abort();
        running
    }
}

impl Drop for ChatTasks {
    fn drop(&mut self) {
        for handle in self.0.values() {
            handle.abort();
        }
    }
}

/// `fut`, or an error naming `what` if it takes longer than `limit`.
async fn within<T>(limit: Duration, what: &str, fut: impl Future<Output = T>) -> Result<T> {
    match tokio::time::timeout(limit, fut).await {
        Ok(value) => Ok(value),
        Err(_) => bail!("{what} timed out after {} ms", limit.as_millis()),
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

/// The next text message during the handshake, skipping control frames.
async fn next_message(
    read: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Result<ServerMessage> {
    loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).context("parsing server message"),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(other)) => bail!("expected text message, got {other:?}"),
            Some(Err(e)) => bail!("websocket error: {e}"),
            None => bail!("connection closed before handshake completed"),
        }
    }
}
