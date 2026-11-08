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
