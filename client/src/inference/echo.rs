//! The `echo` model: streams the user's last message back word by word, so
//! the whole round trip can be tested without a real model. Built in, and
//! offered while the server's `INFERENCE_ECHO_MODEL` is on.

use super::{InferenceError, InferenceEvent};
use log::debug;
use protocol::{Backend, ChatRole, ChatTurn, LocalModel, ModelMeta, ModelState, Stage};
use std::time::Duration;
use tokio::sync::mpsc;

pub const ECHO_MODEL: &str = "echo";

/// Pause between words, so the reply visibly streams like a model's would.
const WORD_DELAY: Duration = Duration::from_millis(50);

/// How it appears in the inventory: always loaded.
pub fn local_model() -> LocalModel {
    LocalModel {
        id: ECHO_MODEL.into(),
        display_name: "Echo (test model)".into(),
        backend: Backend::Echo,
        state: ModelState::Loaded,
        size_bytes: 0,
        context_length: None,
        meta: ModelMeta::default(),
        error: None,
    }
}

pub async fn chat(messages: &[ChatTurn], out: &mpsc::Sender<InferenceEvent>) -> Result<(), InferenceError> {
    let text = messages
        .iter()
        .rev()
        .find(|t| t.role == ChatRole::User)
        .map(|t| t.content.as_str())
        .ok_or(InferenceError::NoUserMessage)?;
    debug!(target: "chat", "echoing {} chars", text.len());
    out.send(InferenceEvent::Status(Stage::Generating))
        .await
        .map_err(|_| InferenceError::Closed)?;
    for (i, piece) in words(text).enumerate() {
        if i > 0 {
            tokio::time::sleep(WORD_DELAY).await;
        }
        out.send(InferenceEvent::Delta(piece.to_string()))
            .await
            .map_err(|_| InferenceError::Closed)?;
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
}
