//! Chat: the dashboard's messages relayed by the server to a client, and
//! the client's reply streamed back in chunks.
//!
//! Each request carries the whole conversation so far (stateless, like
//! the OpenAI and Anthropic APIs), so a client keeps nothing between
//! requests. One `request_id` ties a request to its chunks, its final
//! `chat_done` or `chat_error`, and a possible `chat_cancel`.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

/// One message of the conversation history in a request.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ChatTurn {
    pub role: ChatRole,
    pub content: String,
}

/// A model a client can answer with. A struct rather than a bare string
/// so details (display name, context length, ...) can be added later as
/// `Option` fields without breaking older peers.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
}
