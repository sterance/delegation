mod identity;
mod protocol;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::Signer;
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMessage, ServerMessage};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

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
    format!("{}/agent", ws_base.trim_end_matches('/'))
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
        println!("[ws] connecting to {ws_url} ...");
        match run_session(&ws_url, &identity).await {
            Ok(()) => println!("[ws] session ended cleanly, reconnecting in {backoff:?}"),
            Err(e) => eprintln!("[ws] session error: {e:#}. Reconnecting in {backoff:?}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, Duration::from_secs(30));
    }
}

async fn run_session(ws_url: &str, identity: &identity::Identity) -> Result<()> {
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
        ServerMessage::AuthOk {} => println!("[ws] authenticated"),
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

    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let timestamp = now_ms();
                send(&mut write, &ClientMessage::Heartbeat { timestamp, hostname: hostname.clone() }).await?;
                println!("[heartbeat] sent");
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ServerMessage>(&text) {
                            Ok(ServerMessage::HeartbeatAck {}) => println!("[heartbeat] acked"),
                            Ok(other) => println!("[ws] unexpected message during heartbeat loop: {other:?}"),
                            Err(e) => println!("[ws] couldn't parse server message: {e}"),
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
