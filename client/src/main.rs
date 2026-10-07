mod identity;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::Signer;
use futures_util::{SinkExt, StreamExt};
use protocol::{ts, ClientMessage, ServerMessage, AUTH_FAILED_UNKNOWN_CLIENT};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

/// How long we'll tolerate going without a heartbeat_ack before treating
/// the connection as dead and forcing a reconnect. This is what catches
/// a silently dropped connection (dead wifi, out-of-range client) that
/// never produces a transport-level error — see the hurdles writeup.
const ACK_TIMEOUT: Duration = Duration::from_secs(30);

struct Args {
    server: String, // e.g. "http://localhost:8080" or "https://llm.smith-c.com"
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
        server: server.unwrap_or_else(|| "http://localhost:8080".to_string()),
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
    format!("{}/ws", ws_base.trim_end_matches('/'))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let identity = identity::load_or_enroll(&args.server, args.pairing_code.as_deref())?;
    let ws_url = ws_url_for(&args.server);

    // Basic reconnect-with-backoff loop. This is the piece worth stress
    // testing hardest once this is on a real home connection instead of
    // localhost: kill the tunnel mid-session and confirm this recovers
    // cleanly rather than spinning or silently dying.
    let mut backoff = Duration::from_secs(1);
    loop {
        println!("[{}] [ws] connecting to {ws_url} ...", ts());
        match run_session(&ws_url, &identity).await {
            Ok(()) => println!("[{}] [ws] session ended cleanly, reconnecting in {backoff:?}", ts()),
            Err(e) => eprintln!("[{}] [ws] session error: {e:#}. Reconnecting in {backoff:?}", ts()),
        }
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, Duration::from_secs(30));
    }
}

/// The server has no record of this client_id — most likely its
/// database was reset, or this identity was enrolled against a
/// different server instance entirely. Retrying the handshake can never
/// succeed, so don't loop on it forever: wipe the stale identity and
/// tell the human what to do, rather than spinning with AuthFailed
/// every few seconds (this is exactly the bug hit by hand twice during
/// real deployment, before this fix existed).
fn handle_unknown_client() -> ! {
    identity::clear();
    eprintln!(
        "[{}] server doesn't recognize this client_id — identity cleared. \
         Re-run with --pairing-code to re-enroll.",
        ts()
    );
    std::process::exit(1);
}

async fn run_session(ws_url: &str, identity: &identity::Identity) -> Result<()> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .context("websocket connect failed")?;
    let (mut write, mut read) = ws_stream.split();

    send(&mut write, &ClientMessage::Hello { client_id: identity.client_id.clone() }).await?;

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
        ServerMessage::AuthFailed { reason } if reason == AUTH_FAILED_UNKNOWN_CLIENT => {
            handle_unknown_client()
        }
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
        ServerMessage::AuthFailed { reason } if reason == AUTH_FAILED_UNKNOWN_CLIENT => {
            handle_unknown_client()
        }
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

    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // Self-heal on a silently dead connection: if the transport
                // hasn't told us anything is wrong but we also haven't heard
                // an ack in a while, the server has likely already given up
                // on us (its own heartbeat-timeout sweep) — don't wait for
                // an OS-level TCP error that may never come.
                if last_ack.elapsed() > ACK_TIMEOUT {
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
