use serde::{Deserialize, Serialize};

// Mirrors server/src/protocol.ts by hand for this scaffold. Once this
// grows past a handful of message types, generating both sides from one
// schema (e.g. a small JSON Schema or protobuf file) is worth doing —
// keeping two hand-written copies in sync is a real source of subtle
// bugs (a typo'd field name fails silently as "unexpected message").

#[derive(Serialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "hello")]
    Hello { client_id: String },
    #[serde(rename = "challenge_response")]
    ChallengeResponse { signature: String },
    #[serde(rename = "heartbeat")]
    Heartbeat { timestamp: i64, hostname: String },
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "challenge")]
    Challenge { nonce: String },
    #[serde(rename = "auth_ok")]
    AuthOk {},
    #[serde(rename = "auth_failed")]
    AuthFailed { reason: String },
    #[serde(rename = "heartbeat_ack")]
    HeartbeatAck {},
}
