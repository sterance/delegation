//! Wire protocol shared by the delegation server and client.
//!
//! This used to be two hand-maintained copies (`protocol.ts` on the
//! server, `protocol.rs` on the client) that had to be kept in sync by
//! eye. Now that both sides are Rust, there's exactly one definition —
//! a typo'd field name becomes a compile error on whichever side you
//! didn't update, instead of a silent "unexpected message" at runtime.

use serde::{Deserialize, Serialize};

pub mod config;
pub mod hardware;

pub use hardware::HardwareSpecs;

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
    /// Sent once after every successful `auth_ok`, and again if the
    /// client's hardware scan finishes later than that. Not acked. The
    /// hostname lives here rather than in every heartbeat so a renamed
    /// machine shows its new name without re-enrolling.
    #[serde(rename = "info")]
    Info {
        hostname: String,
        os: String,
        client_version: String,
        specs: HardwareSpecs,
    },
    /// Old clients also send a `hostname` field here; serde ignores it, so
    /// they keep working against a new server.
    #[serde(rename = "heartbeat")]
    Heartbeat { timestamp: i64 },
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
    AuthFailed { reason: AuthFailedReason },
    #[serde(rename = "heartbeat_ack")]
    HeartbeatAck {},
}

/// Why the server rejected a handshake. Serialized as the same strings
/// the protocol has always used. The client matches on `UnknownClient` to
/// decide whether to wipe its local identity and fall back to asking for a
/// new pairing code, rather than looping forever on a handshake that can
/// never succeed (the "server has forgotten this client_id" case).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailedReason {
    #[serde(rename = "unknown client_id")]
    UnknownClient,
    #[serde(rename = "signature verification failed")]
    BadSignature,
    /// Anything a newer server might send that this build doesn't know.
    #[serde(other)]
    Other,
}

impl std::fmt::Display for AuthFailedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AuthFailedReason::UnknownClient => "unknown client_id",
            AuthFailedReason::BadSignature => "signature verification failed",
            AuthFailedReason::Other => "unrecognized reason",
        })
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_heartbeat_with_hostname_still_parses() {
        let msg: ClientMessage =
            serde_json::from_str(r#"{"type":"heartbeat","timestamp":1,"hostname":"old-client"}"#).unwrap();
        assert!(matches!(msg, ClientMessage::Heartbeat { timestamp: 1 }));
    }

    #[test]
    fn auth_failed_reasons_keep_their_wire_strings() {
        let json = serde_json::to_string(&ServerMessage::AuthFailed {
            reason: AuthFailedReason::UnknownClient,
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"auth_failed","reason":"unknown client_id"}"#);
        let json = serde_json::to_string(&ServerMessage::AuthFailed {
            reason: AuthFailedReason::BadSignature,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"auth_failed","reason":"signature verification failed"}"#
        );
        let msg: ServerMessage = serde_json::from_str(r#"{"type":"auth_failed","reason":"something new"}"#).unwrap();
        assert!(matches!(
            msg,
            ServerMessage::AuthFailed {
                reason: AuthFailedReason::Other
            }
        ));
    }

    #[test]
    fn info_round_trips_with_missing_spec_fields() {
        let msg: ClientMessage = serde_json::from_str(
            r#"{"type":"info","hostname":"h","os":"Linux","client_version":"0.1.0",
                "specs":{"cpu":{"model":"X"},"disks":[{"media":"ssd","interface":"thunderbolt"}]}}"#,
        )
        .unwrap();
        let ClientMessage::Info { specs, .. } = msg else {
            panic!("not info")
        };
        assert_eq!(specs.cpu.unwrap().model.as_deref(), Some("X"));
        assert!(specs.memory.is_none());
        assert!(specs.gpus.is_empty());
        assert_eq!(specs.disks[0].media, hardware::DiskMedia::Ssd);
        assert_eq!(specs.disks[0].interface, hardware::DiskInterface::Other);
    }
}
