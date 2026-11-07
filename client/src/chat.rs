//! Answers chat requests relayed by the server.
//!
//! **This is the LLM plug-in point.** The session calls `models()` once per
//! connection to advertise what this client offers, and `respond()` once
//! per request, as its own task. A real backend only needs to replace
//! these two functions: stream its output as `ChatChunk`s and finish with
//! `ChatDone` or `ChatError`. Cancellation is handled by the session, which
//! aborts the task, so `respond` needs no cancellation logic of its own.
//!
//! For now the only model is `echo`, which streams the user's last message
//! back word by word, so the whole round trip can be tested without one.

use log::debug;
use protocol::{ChatRole, ChatTurn, ClientMessage, ModelInfo};
use std::time::Duration;
use tokio::sync::mpsc;

pub const ECHO_MODEL: &str = "echo";

/// Pause between words, so the reply visibly streams like a model's would.
const ECHO_WORD_DELAY: Duration = Duration::from_millis(50);

/// The models this client offers, advertised after every `welcome`.
pub fn models() -> Vec<ModelInfo> {
    vec![ModelInfo { id: ECHO_MODEL.into() }]
}

/// Generates the reply to one request and sends it on `out`, ending with
/// exactly one `ChatDone` or `ChatError`. Returns early if the session has
/// closed `out`.
pub async fn respond(request_id: String, model: String, messages: Vec<ChatTurn>, out: mpsc::Sender<ClientMessage>) {
    let result = match model.as_str() {
        ECHO_MODEL => echo(&request_id, &messages, &out).await,
        other => Err(format!("this client doesn't offer the model {other:?}")),
    };
    let last = match result {
        Ok(()) => ClientMessage::ChatDone { request_id },
        Err(error) => ClientMessage::ChatError { request_id, error },
    };
    let _ = out.send(last).await;
}

async fn echo(request_id: &str, messages: &[ChatTurn], out: &mpsc::Sender<ClientMessage>) -> Result<(), String> {
    let text = messages
        .iter()
        .rev()
        .find(|t| t.role == ChatRole::User)
        .map(|t| t.content.as_str())
        .ok_or("there's no user message to echo")?;
    debug!(target: "chat", "{request_id}: echoing {} chars", text.len());
    for (i, piece) in words(text).enumerate() {
        if i > 0 {
            tokio::time::sleep(ECHO_WORD_DELAY).await;
        }
        let chunk = ClientMessage::ChatChunk {
            request_id: request_id.to_string(),
            delta: piece.to_string(),
        };
        out.send(chunk).await.map_err(|_| "the session closed".to_string())?;
    }
    Ok(())
}

/// Splits after each run of whitespace, so the pieces join back into the
/// exact input (newlines and repeated spaces included).
fn words(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let after_space = rest[word_end..]
            .find(|c: char| !c.is_whitespace())
            .map_or(rest.len(), |i| word_end + i);
        let (piece, tail) = rest.split_at(after_space);
        rest = tail;
        Some(piece)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: ChatRole, content: &str) -> ChatTurn {
        ChatTurn {
            role,
            content: content.into(),
        }
    }

    async fn run(model: &str, messages: Vec<ChatTurn>) -> Vec<ClientMessage> {
        let (tx, mut rx) = mpsc::channel(64);
        respond("r".into(), model.into(), messages, tx).await;
        let mut got = Vec::new();
        while let Some(msg) = rx.recv().await {
            got.push(msg);
        }
        got
    }

    #[test]
    fn words_rejoin_into_the_input() {
        for text in ["one", "two words", "  leading", "trailing  ", "a\n\nb \t c", ""] {
            assert_eq!(words(text).collect::<String>(), text);
        }
        assert_eq!(
            words("hi there\nyou").collect::<Vec<_>>(),
            vec!["hi ", "there\n", "you"]
        );
    }

    #[tokio::test]
    async fn echo_streams_the_last_user_message_then_finishes() {
        let got = run(
            ECHO_MODEL,
            vec![
                turn(ChatRole::User, "first"),
                turn(ChatRole::Assistant, "first"),
                turn(ChatRole::User, "hello there\nworld"),
            ],
        )
        .await;
        let (last, chunks) = got.split_last().unwrap();
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

    #[tokio::test]
    async fn unknown_model_or_no_user_message_is_an_error() {
        let got = run("nope", vec![turn(ChatRole::User, "hi")]).await;
        assert!(matches!(got.as_slice(), [ClientMessage::ChatError { error, .. }] if error.contains("nope")));
        let got = run(ECHO_MODEL, vec![turn(ChatRole::Assistant, "hi")]).await;
        assert!(matches!(got.as_slice(), [ClientMessage::ChatError { .. }]));
    }
}
