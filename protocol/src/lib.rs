//! Wire protocol shared by the delegation server and client.
//!
//! This used to be two hand-maintained copies (`protocol.ts` on the
//! server, `protocol.rs` on the client) that had to be kept in sync by
//! eye. Now that both sides are Rust, there's exactly one definition —
//! a typo'd field name becomes a compile error on whichever side you
//! didn't update, instead of a silent "unexpected message" at runtime.

use serde::{Deserialize, Serialize};

// ---- enrollment (HTTP) ----

#[derive(Serialize, Deserialize)]
pub struct EnrollRequest {
    pub pairing_code: String,
    pub public_key: String, // base64, 32-byte Ed25519 public key
    pub hostname: String,
}

#[derive(Serialize, Deserialize)]
pub struct EnrollResponse {
    pub client_id: String,
}

#[derive(Serialize, Deserialize)]
pub struct EnrollError {
    pub error: String,
}

// ---- WebSocket messages, client -> server ----

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "hello")]
    Hello { client_id: String },
    #[serde(rename = "challenge_response")]
    ChallengeResponse { signature: String },
    #[serde(rename = "heartbeat")]
    Heartbeat { timestamp: i64, hostname: String },
}

// ---- WebSocket messages, server -> client ----

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "challenge")]
    Challenge { nonce: String }, // base64, 16 random bytes
    #[serde(rename = "auth_ok")]
    AuthOk {},
    #[serde(rename = "auth_failed")]
    AuthFailed { reason: String },
    #[serde(rename = "heartbeat_ack")]
    HeartbeatAck {},
}

/// A machine-readable reason string for auth failure. The client
/// pattern-matches on this specific value to decide whether to wipe its
/// local identity and fall back to asking for a new pairing code, rather
/// than looping forever on a handshake that can never succeed (see the
/// "server has forgotten this client_id" case from the planning notes).
pub const AUTH_FAILED_UNKNOWN_CLIENT: &str = "unknown client_id";

/// UTC timestamp for log lines, e.g. "2026-09-29T23:31:05Z". Hand-rolled
/// (Howard Hinnant's civil_from_days algorithm) rather than pulling in a
/// date/time crate — shared here so both binaries get identical,
/// correlatable log timestamps for free.
pub fn ts() -> String {
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
