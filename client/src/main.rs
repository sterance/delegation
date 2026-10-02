mod identity;
mod protocol;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::Signer;
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMessage, ServerMessage};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

struct Config {
    ack_timeout: Duration,
    heartbeat_interval: Duration,
    reconnect_initial_backoff: Duration,
    reconnect_max_backoff: Duration,
}

impl Config {
    fn from_env() -> Result<Self> {
        let config = Self {
            ack_timeout: duration_from_env("CLIENT_ACK_TIMEOUT_MS", 30_000)?,
            heartbeat_interval: duration_from_env("CLIENT_HEARTBEAT_INTERVAL_MS", 10_000)?,
            reconnect_initial_backoff: duration_from_env("CLIENT_RECONNECT_INITIAL_BACKOFF_MS", 1_000)?,
            reconnect_max_backoff: duration_from_env("CLIENT_RECONNECT_MAX_BACKOFF_MS", 30_000)?,
        };
        if config.reconnect_initial_backoff > config.reconnect_max_backoff {
            bail!("CLIENT_RECONNECT_INITIAL_BACKOFF_MS must not exceed CLIENT_RECONNECT_MAX_BACKOFF_MS");
        }
        Ok(config)
    }
}

fn duration_from_env(name: &str, fallback_ms: u64) -> Result<Duration> {
    let value = match std::env::var(name) {
        Ok(raw) => raw
            .parse::<u64>()
            .with_context(|| format!("{name} must be a positive integer number of milliseconds"))?,
        Err(std::env::VarError::NotPresent) => fallback_ms,
        Err(err) => bail!("could not read {name}: {err}"),
    };
    if value == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(Duration::from_millis(value))
}

/// UTC timestamp for log lines, e.g. "2026-09-29T23:31:05Z". Hand-rolled
/// (Howard Hinnant's civil_from_days algorithm) rather than pulling in a
/// date/time crate, in keeping with this agent's minimal-dependency goal.
pub(crate) fn ts() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = (secs / 86400) as i64;
    let tod = secs % 86400;
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

struct Args {
    server: String, // e.g. "http://localhost:7070" or "https://llm.smith-c.com"
    pairing_code: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut server = None;
    let mut pairing_code = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" => server = args.next(),
            "--pairing-code" => pairing_code = args.next(),
            other => bail!("unknown argument: {other}"),
        }
    }
    Ok(Args {
        server: server.unwrap_or_else(|| "http://localhost:7070".to_string()),
        pairing_code,
    })
}

fn ws_url_for(server_http_base: &str) -> String {
    let ws_base = if let Some(rest) = server_http_base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = server_http_base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        server_http_base.to_string()
    };
    format!("{}/agent", ws_base.trim_end_matches('/'))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let config = Config::from_env()?;
    let identity = identity::load_or_enroll(&args.server, args.pairing_code.as_deref())?;
    let ws_url = ws_url_for(&args.server);

    // Basic reconnect-with-backoff loop. This is the piece worth stress
    // testing hardest once this is on a real home connection instead of
    // localhost: kill the tunnel mid-session and confirm this recovers
    // cleanly rather than spinning or silently dying.
    let mut backoff = config.reconnect_initial_backoff;
    loop {
        println!("[{}] [ws] connecting to {ws_url} ...", ts());
        match run_session(&ws_url, &identity, &config).await {
            Ok(()) => println!("[{}] [ws] session ended cleanly, reconnecting in {backoff:?}", ts()),
            Err(e) => eprintln!("[{}] [ws] session error: {e:#}. Reconnecting in {backoff:?}", ts()),
        }
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, config.reconnect_max_backoff);
    }
}

async fn run_session(ws_url: &str, identity: &identity::Identity, config: &Config) -> Result<()> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .context("websocket connect failed")?;
    let (mut write, mut read) = ws_stream.split();

    send(&mut write, &ClientMessage::Hello { client_id: identity.client_id.clone() }).await?;

    // --- handshake: expect a challenge, then send a signed response ---
    let challenge_msg = next_message(&mut read).await?;
    let nonce = match challenge_msg {
        ServerMessage::Challenge { nonce } => nonce,
        other => bail!("expected challenge, got {other:?}"),
    };
    let nonce_bytes = STANDARD.decode(&nonce).context("decoding nonce")?;
    let signature = identity.signing_key.sign(&nonce_bytes);
    send(
        &mut write,
        &ClientMessage::ChallengeResponse { signature: STANDARD.encode(signature.to_bytes()) },
    )
    .await?;

    let auth_result = next_message(&mut read).await?;
    match auth_result {
        ServerMessage::AuthOk {} => println!("[{}] [ws] authenticated", ts()),
        ServerMessage::AuthFailed { reason } => bail!("server rejected auth: {reason}"),
        other => bail!("expected auth_ok, got {other:?}"),
    }

    // --- authenticated: heartbeat loop ---
    let hostname = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown-host".to_string());

    // Baseline set right after auth, so a server that never acks even the
    // first heartbeat gets caught by the same timeout rather than waiting
    // indefinitely for a "last" ack that never came.
    let mut last_ack = Instant::now();

    let mut ticker = tokio::time::interval(config.heartbeat_interval);
    loop {
        tokio::select! {
             _ = ticker.tick() => {
                // Self-heal on a silently dead connection: if the transport
                // hasn't told us anything is wrong but we also haven't heard
                // an ack in a while, the server has likely already given up
                // on us (its own heartbeat-timeout sweep) — don't wait for
                // an OS-level TCP error that may never come.
                if last_ack.elapsed() > config.ack_timeout {
                    bail!("no heartbeat_ack received in {:?}, treating connection as dead", last_ack.elapsed());
                }
                let timestamp = now_ms();
                send(&mut write, &ClientMessage::Heartbeat { timestamp, hostname: hostname.clone() }).await?;
                println!("[{}] [heartbeat] sent", ts());
             }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ServerMessage>(&text) {
                            Ok(ServerMessage::HeartbeatAck {}) => {
                                last_ack = Instant::now();
                                println!("[{}] [heartbeat] acked", ts());
                            }
                            Ok(other) => println!("[{}] [ws] unexpected message during heartbeat loop: {other:?}", ts()),
                            Err(e) => println!("[{}] [ws] couldn't parse server message: {e}", ts()),
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

async fn send(
    write: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    msg: &ClientMessage,
) -> Result<()> {
    let text = serde_json::to_string(msg)?;
    write.send(Message::Text(text)).await.context("sending websocket message")
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
