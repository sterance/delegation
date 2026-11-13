//! The session controller against an in-process stand-in for the server:
//! a bare WebSocket listener that runs the handshake (without checking the
//! signature) and then watches what the client sends.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use delegation_client::identity::Identity;
use delegation_client::inference::echo::ECHO_MODEL;
use delegation_client::inference::Inference;
use delegation_client::sampler::{Reading, Sampler, Source};
use delegation_client::session::{self, ClientInfo, ServerIdentity, SessionCmd, SessionStatus};
use futures_util::{SinkExt, StreamExt};
use protocol::{
    ChatRole, ChatTurn, ClientMessage, ClientSettings, CommandState, InferenceSettings, ModelCommandKind,
    ServerMessage, Stage, Tick,
};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{accept_async, tungstenite::Message, WebSocketStream};

type Ws = WebSocketStream<TcpStream>;

fn fast_settings() -> ClientSettings {
    ClientSettings {
        tick_interval_ms: 50,
        sampler_interval_ms: 200,
        sample_max_age_ms: 400,
        server_silence_timeout_ms: 400,
        reconnect_backoff_initial_ms: 50,
        reconnect_backoff_max_ms: 100,
        inference: InferenceSettings {
            echo_model_enabled: true,
            ..InferenceSettings::default()
        },
    }
}

/// A data directory for the test's `Inference`, removed on drop.
struct DataDir(std::path::PathBuf);

impl DataDir {
    fn new() -> DataDir {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "delegation-session-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        DataDir(dir)
    }
}

impl Drop for DataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One reading, then hangs like a wedged GPU driver.
struct StallsAfterFirst(bool);

impl Source for StallsAfterFirst {
    fn read(&mut self) -> Option<Reading> {
        if std::mem::replace(&mut self.0, true) {
            std::thread::sleep(Duration::from_secs(3600));
        }
        Some(Reading {
            cpu_percent: 12.0,
            ram_total_bytes: 100,
            ram_available_bytes: 50,
            swap_total_bytes: None,
            swap_used_bytes: None,
            gpus: vec![],
        })
    }
}

struct Harness {
    listener: TcpListener,
    statuses: mpsc::UnboundedReceiver<SessionStatus>,
    _cmds: mpsc::Sender<SessionCmd>,
    _machine: watch::Sender<Option<std::sync::Arc<delegation_client::hardware::MachineInfo>>>,
    _data: DataDir,
}

/// Starts a controller connected to a fresh local listener.
async fn start(sampler: Sampler) -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (machine_tx, machine_rx) = watch::channel(None);
    let (cmd_tx, cmd_rx) = mpsc::channel(4);
    let (status_tx, status_rx) = mpsc::unbounded_channel();
    let data = DataDir::new();
    tokio::spawn(session::controller(
        cmd_rx,
        move |_url: &str, status| {
            let _ = status_tx.send(status);
        },
        ClientInfo {
            machine: machine_rx,
            sampler,
            inference: Inference::new(&data.0),
        },
    ));
    let identity = Identity::from_b64("test-client".into(), &STANDARD.encode([7u8; 32])).unwrap();
    cmd_tx
        .send(SessionCmd::Connect(ServerIdentity {
            url,
            identity,
            // Cached settings: these drive the first connection's timeouts.
            settings: fast_settings(),
        }))
        .await
        .unwrap();
    Harness {
        listener,
        statuses: status_rx,
        _cmds: cmd_tx,
        _machine: machine_tx,
        _data: data,
    }
}

async fn next_client_message(ws: &mut Ws) -> ClientMessage {
    loop {
        match ws.next().await.expect("client hung up").expect("websocket error") {
            Message::Text(text) => return serde_json::from_str(&text).expect("unparseable client message"),
            Message::Close(_) => panic!("client closed the connection"),
            _ => {}
        }
    }
}

async fn send(ws: &mut Ws, msg: &ServerMessage) {
    ws.send(Message::Text(serde_json::to_string(msg).unwrap().into()))
        .await
        .unwrap();
}

/// Accepts the next connection and runs the handshake up to `welcome`.
async fn accept_and_welcome(listener: &TcpListener, settings: ClientSettings) -> Ws {
    let (tcp, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("client didn't connect")
        .unwrap();
    let mut ws = accept_async(tcp).await.unwrap();
    assert!(matches!(
        next_client_message(&mut ws).await,
        ClientMessage::Hello { .. }
    ));
    send(
        &mut ws,
        &ServerMessage::Challenge {
            nonce: STANDARD.encode([0u8; 16]),
        },
    )
    .await;
    assert!(matches!(
        next_client_message(&mut ws).await,
        ClientMessage::ChallengeResponse { .. }
    ));
    send(&mut ws, &ServerMessage::Welcome { settings }).await;
    ws
}

async fn expect_status(statuses: &mut mpsc::UnboundedReceiver<SessionStatus>) -> SessionStatus {
    tokio::time::timeout(Duration::from_secs(5), statuses.recv())
        .await
        .expect("no status update")
        .expect("controller stopped")
}

#[tokio::test]
async fn ticks_keep_coming_without_metrics_when_the_sampler_stalls() {
    let sampler = Sampler::spawn_with(Duration::from_millis(200), || StallsAfterFirst(false));
    let mut h = start(sampler).await;
    let ws = accept_and_welcome(&h.listener, fast_settings()).await;
    assert_eq!(expect_status(&mut h.statuses).await, SessionStatus::Connecting);
    assert_eq!(
        expect_status(&mut h.statuses).await,
        SessionStatus::Connected(fast_settings())
    );

    let (mut sink, mut stream) = ws.split();
    // The server's pings: without them the client would (rightly) give up
    // after its 400 ms silence timeout.
    let pinger = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                return;
            }
        }
    });

    let mut ticks: Vec<Tick> = Vec::new();
    let mut inventories = Vec::new();
    let mut pongs = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1_500);
    while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, stream.next()).await {
        match frame.unwrap() {
            Message::Text(text) => match serde_json::from_str(&text).unwrap() {
                ClientMessage::Tick(tick) => ticks.push(tick),
                ClientMessage::ModelInventory(inventory) => inventories.push(inventory),
                other => panic!("unexpected {other:?}"),
            },
            Message::Pong(_) => pongs += 1,
            other => panic!("unexpected frame {other:?}"),
        }
    }
    pinger.abort();

    // ~30 ticks at 50 ms; allow for a slow CI machine.
    assert!(ticks.len() >= 15, "only {} ticks in 1.5 s", ticks.len());
    let seqs: Vec<u64> = ticks.iter().map(|t| t.seq).collect();
    assert_eq!(
        seqs,
        (0..ticks.len() as u64).collect::<Vec<_>>(),
        "seq counts up from 0 with no gaps"
    );
    let with_metrics: Vec<&Tick> = ticks.iter().filter(|t| t.metrics.is_some()).collect();
    assert!(!with_metrics.is_empty(), "the one reading made it into some ticks");
    assert!(with_metrics
        .iter()
        .all(|t| t.metrics.as_ref().unwrap().sample_age_ms <= 400));
    assert!(
        ticks.iter().rev().take(10).all(|t| t.metrics.is_none()),
        "once the reading is older than sample_max_age_ms, ticks carry no metrics"
    );
    assert!(pongs >= 5, "pings were answered with pongs ({pongs})");
    assert_eq!(
        inventories.len(),
        1,
        "the inventory is sent once per connection when nothing changes"
    );
    let ids: Vec<&str> = inventories[0].models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec![ECHO_MODEL]);
    assert!(
        h.statuses.try_recv().is_err(),
        "the client never dropped the connection"
    );
}

#[tokio::test]
async fn a_silent_server_is_dropped_and_reconnected() {
    let sampler = Sampler::spawn_with(Duration::from_millis(200), || StallsAfterFirst(false));
    let mut h = start(sampler).await;
    let mut first = accept_and_welcome(&h.listener, fast_settings()).await;
    assert_eq!(expect_status(&mut h.statuses).await, SessionStatus::Connecting);
    assert!(matches!(
        expect_status(&mut h.statuses).await,
        SessionStatus::Connected(_)
    ));

    // Read the client's ticks but never send anything back.
    let silent = tokio::spawn(async move { while let Some(Ok(_)) = first.next().await {} });

    let SessionStatus::Disconnected { error: Some(error) } = expect_status(&mut h.statuses).await else {
        panic!("expected a disconnect with an error");
    };
    assert!(error.contains("nothing from the server"), "{error}");
    assert_eq!(expect_status(&mut h.statuses).await, SessionStatus::Connecting);

    // Reconnects after the (50 ms) backoff, and takes the new welcome's
    // settings.
    let pushed = ClientSettings {
        tick_interval_ms: 80,
        ..fast_settings()
    };
    let _second = accept_and_welcome(&h.listener, pushed).await;
    assert_eq!(expect_status(&mut h.statuses).await, SessionStatus::Connected(pushed));
    silent.abort();
}

/// The chat and command messages the client sends, skipping ticks,
/// inventories and progress.
async fn next_chat_message(ws: &mut Ws) -> ClientMessage {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), next_client_message(ws))
            .await
            .expect("no chat message")
        {
            ClientMessage::Tick(_)
            | ClientMessage::ModelInventory(_)
            | ClientMessage::Info { .. }
            | ClientMessage::ChatProgress { .. } => {}
            other => return other,
        }
    }
}

fn echo_request(request_id: &str, text: &str) -> ServerMessage {
    ServerMessage::ChatRequest {
        request_id: request_id.into(),
        model: ECHO_MODEL.into(),
        messages: vec![ChatTurn {
            role: ChatRole::User,
            content: text.into(),
        }],
        temperature: None,
    }
}

#[tokio::test]
async fn chat_requests_are_echoed_back_and_can_be_cancelled() {
    let sampler = Sampler::spawn_with(Duration::from_millis(200), || StallsAfterFirst(false));
    let h = start(sampler).await;
    // Slow ticks and a long silence timeout: this test doesn't send pings.
    let settings = ClientSettings {
        tick_interval_ms: 1_000,
        server_silence_timeout_ms: 10_000,
        ..fast_settings()
    };
    let mut ws = accept_and_welcome(&h.listener, settings).await;

    send(&mut ws, &echo_request("r1", "hello there world")).await;
    let mut reply = String::new();
    loop {
        match next_chat_message(&mut ws).await {
            ClientMessage::ChatChunk { request_id, delta } => {
                assert_eq!(request_id, "r1");
                reply.push_str(&delta);
            }
            ClientMessage::ChatDone { request_id, .. } => {
                assert_eq!(request_id, "r1");
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(reply, "hello there world");

    // Cancelled after the first word: no more of r2 arrives.
    let long: String = (0..40).map(|i| format!("w{i} ")).collect();
    send(&mut ws, &echo_request("r2", &long)).await;
    assert!(matches!(
        next_chat_message(&mut ws).await,
        ClientMessage::ChatChunk { request_id, .. } if request_id == "r2"
    ));
    send(
        &mut ws,
        &ServerMessage::ChatCancel {
            request_id: "r2".into(),
        },
    )
    .await;
    // At most one chunk was already in flight; after that, nothing.
    let mut after_cancel = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while let Ok(msg) = tokio::time::timeout_at(deadline, next_client_message(&mut ws)).await {
        match msg {
            ClientMessage::ChatChunk { .. } => after_cancel += 1,
            ClientMessage::ChatDone { .. } | ClientMessage::ChatError { .. } => panic!("a cancelled reply finished"),
            _ => {}
        }
    }
    assert!(after_cancel <= 1, "{after_cancel} chunks arrived after the cancel");
}

#[tokio::test]
async fn replies_report_progress_first_and_model_commands_are_answered() {
    let sampler = Sampler::spawn_with(Duration::from_millis(200), || StallsAfterFirst(false));
    let h = start(sampler).await;
    let settings = ClientSettings {
        tick_interval_ms: 1_000,
        server_silence_timeout_ms: 10_000,
        ..fast_settings()
    };
    let mut ws = accept_and_welcome(&h.listener, settings).await;

    // chat_progress comes before any text.
    send(&mut ws, &echo_request("r1", "hi")).await;
    let first = loop {
        match tokio::time::timeout(Duration::from_secs(5), next_client_message(&mut ws))
            .await
            .unwrap()
        {
            ClientMessage::ChatProgress { request_id, stage } => break (request_id, stage),
            ClientMessage::ChatChunk { .. } => panic!("text before chat_progress"),
            _ => {}
        }
    };
    assert_eq!(first, ("r1".to_string(), Stage::Generating));
    while !matches!(next_chat_message(&mut ws).await, ClientMessage::ChatDone { .. }) {}

    // A command kind from a newer server.
    send(
        &mut ws,
        &ServerMessage::ModelCommand {
            command_id: "c1".into(),
            command: ModelCommandKind::Unknown,
        },
    )
    .await;
    match next_chat_message(&mut ws).await {
        ClientMessage::ModelCommandStatus { command_id, state, .. } => {
            assert_eq!((command_id.as_str(), state), ("c1", CommandState::Unsupported))
        }
        other => panic!("unexpected {other:?}"),
    }
    // An invalid pull fails without touching anything.
    let raw = r#"{"type":"model_command","command_id":"c2","command":{"kind":"pull","model":{"id":"../x","display_name":"x","backend":"llama_cpp","source":{"url":"https://example.com/x","sha256":"00","size_bytes":1}}}}"#;
    ws.send(Message::Text(raw.into())).await.unwrap();
    match next_chat_message(&mut ws).await {
        ClientMessage::ModelCommandStatus {
            command_id,
            state,
            error,
            ..
        } => {
            assert_eq!((command_id.as_str(), state), ("c2", CommandState::Failed));
            assert!(error.unwrap().contains("valid model id"));
        }
        other => panic!("unexpected {other:?}"),
    }
}
