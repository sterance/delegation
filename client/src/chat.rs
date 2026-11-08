//! Chat requests relayed by the server, translated to and from
//! [`Inference::chat`]: `chat_request` in; `chat_progress`, `chat_chunk`,
//! then `chat_done` or `chat_error` out. Nothing about models lives here.
//!
//! Cancellation is the session's job: it aborts the task running
//! `respond`, which drops the `Inference::chat` future, which stops
//! generation.

use crate::inference::{ChatCompletion, Inference, InferenceEvent};
use log::debug;
use protocol::{ChatTurn, ClientMessage, Stage};
use std::time::Duration;
use tokio::sync::mpsc;

/// Until text flows, the current stage is repeated this often. A model can
/// take longer to load than the server's `CHAT_IDLE_TIMEOUT_MS`, and every
/// `chat_progress` counts as activity there.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// Turns inference events into messages: every delta, and each stage once
/// (plus heartbeats until the first delta).
struct Progress {
    request_id: String,
    stage: Option<Stage>,
    texting: bool,
}

impl Progress {
    fn new(request_id: String) -> Self {
        Progress {
            request_id,
            stage: None,
            texting: false,
        }
    }

    fn message(&mut self, event: InferenceEvent) -> Option<ClientMessage> {
        match event {
            InferenceEvent::Delta(delta) => {
                self.texting = true;
                Some(ClientMessage::ChatChunk {
                    request_id: self.request_id.clone(),
                    delta,
                })
            }
            InferenceEvent::Status(stage) if self.stage == Some(stage) => None,
            InferenceEvent::Status(stage) => {
                self.stage = Some(stage);
                Some(self.progress(stage))
            }
        }
    }

    fn heartbeat(&self) -> Option<ClientMessage> {
        (!self.texting).then_some(self.stage?).map(|stage| self.progress(stage))
    }

    fn progress(&self, stage: Stage) -> ClientMessage {
        ClientMessage::ChatProgress {
            request_id: self.request_id.clone(),
            stage,
        }
    }
}

/// Generates the reply to one request and sends it on `out`, ending with
/// exactly one `ChatDone` or `ChatError`. Returns early if the session has
/// closed `out`.
pub async fn respond(
    request_id: String,
    model: String,
    messages: Vec<ChatTurn>,
    temperature: Option<f32>,
    inference: Inference,
    out: mpsc::Sender<ClientMessage>,
) {
    let (events_tx, mut events) = mpsc::channel(64);
    let request = ChatCompletion {
        model_id: model,
        messages,
        // INFERENCE_MAX_TOKENS: the server doesn't set one per request (yet).
        max_tokens: None,
        temperature,
    };
    let chat = inference.chat(request, events_tx);
    tokio::pin!(chat);
    let mut progress = Progress::new(request_id.clone());
    let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    let result = loop {
        tokio::select! {
            biased;
            Some(event) = events.recv() => {
                if let Some(msg) = progress.message(event) {
                    if out.send(msg).await.is_err() {
                        return;
                    }
                }
            }
            result = &mut chat => break result,
            _ = heartbeat.tick() => {
                if let Some(msg) = progress.heartbeat() {
                    if out.send(msg).await.is_err() {
                        return;
                    }
                }
            }
        }
    };
    // What was queued as it finished (all of it, if it finished at once).
    while let Ok(event) = events.try_recv() {
        if let Some(msg) = progress.message(event) {
            if out.send(msg).await.is_err() {
                return;
            }
        }
    }
    let last = match result {
        Ok(()) => ClientMessage::ChatDone { request_id },
        Err(error) => {
            debug!(target: "chat", "{request_id} failed: {error}");
            ClientMessage::ChatError {
                request_id,
                error: error.to_string(),
            }
        }
    };
    let _ = out.send(last).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::store::tests::TempDir;
    use protocol::{ChatRole, Stage};

    fn turn(role: ChatRole, content: &str) -> ChatTurn {
        ChatTurn {
            role,
            content: content.into(),
        }
    }

    async fn run(model: &str, messages: Vec<ChatTurn>) -> Vec<ClientMessage> {
        let dir = TempDir::new("chat");
        let inference = Inference::new(&dir.0);
        let (tx, mut rx) = mpsc::channel(64);
        respond("r".into(), model.into(), messages, None, inference, tx).await;
        let mut got = Vec::new();
        while let Some(msg) = rx.recv().await {
            got.push(msg);
        }
        got
    }

    #[tokio::test]
    async fn echo_streams_the_last_user_message_then_finishes() {
        let got = run(
            "echo",
            vec![
                turn(ChatRole::User, "first"),
                turn(ChatRole::Assistant, "first"),
                turn(ChatRole::User, "hello there\nworld"),
            ],
        )
        .await;
        assert!(matches!(
            &got[0],
            ClientMessage::ChatProgress { request_id, stage: Stage::Generating } if request_id == "r"
        ));
        let (last, chunks) = got[1..].split_last().unwrap();
        assert!(matches!(last, ClientMessage::ChatDone { request_id } if request_id == "r"));
        let text: String = chunks
            .iter()
            .map(|m| match m {
                ClientMessage::ChatChunk { delta, .. } => delta.as_str(),
                other => panic!("expected a chunk, got {other:?}"),
            })
            .collect();
        assert_eq!(text, "hello there\nworld");
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn stages_are_sent_once_and_repeated_only_until_text_arrives() {
        let mut p = Progress::new("r".into());
        assert!(p.heartbeat().is_none(), "nothing to repeat yet");
        assert!(p.message(InferenceEvent::Status(Stage::LoadingModel)).is_some());
        assert!(p.message(InferenceEvent::Status(Stage::LoadingModel)).is_none());
        assert!(matches!(
            p.heartbeat(),
            Some(ClientMessage::ChatProgress {
                stage: Stage::LoadingModel,
                ..
            })
        ));
        assert!(p.message(InferenceEvent::Status(Stage::Generating)).is_some());
        assert!(p.message(InferenceEvent::Delta("x".into())).is_some());
        assert!(p.heartbeat().is_none(), "chunks are the activity now");
    }

    #[tokio::test]
    async fn inference_errors_become_readable_chat_errors() {
        let got = run("nope", vec![turn(ChatRole::User, "hi")]).await;
        assert!(
            matches!(got.as_slice(), [ClientMessage::ChatError { error, .. }] if error.contains("doesn't have the model \"nope\"")),
            "{got:?}"
        );
        let got = run("echo", vec![turn(ChatRole::Assistant, "hi")]).await;
        assert!(
            matches!(got.as_slice(), [ClientMessage::ChatError { error, .. }] if error.contains("no user message")),
            "{got:?}"
        );
    }
}
