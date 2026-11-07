//! Wire protocol shared by the delegation server and client.
//!
//! This used to be two hand-maintained copies (`protocol.ts` on the
//! server, `protocol.rs` on the client) that had to be kept in sync by
//! eye. Now that both sides are Rust, there's exactly one definition —
//! a typo'd field name becomes a compile error on whichever side you
//! didn't update, instead of a silent "unexpected message" at runtime.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub mod hardware;
pub mod telemetry;

pub use hardware::HardwareSpecs;
pub use telemetry::{ClientSettings, GpuMetrics, Metrics, Tick};

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
    /// Sent once after every `welcome`, and again if the client's hardware
    /// scan finishes later than that. Not acked. The hostname lives here
    /// rather than in every tick so a renamed machine shows its new name
    /// without re-enrolling.
    #[serde(rename = "info")]
    Info {
        hostname: String,
        os: String,
        client_version: String,
        specs: HardwareSpecs,
    },
    /// Every `tick_interval_ms` once `welcome` has arrived, with the latest
    /// resource metrics. Also the liveness signal; never acked.
    #[serde(rename = "tick")]
    Tick(Tick),
}

// ---- WebSocket messages, server -> client ----

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "challenge")]
    Challenge { nonce: String }, // base64, 16 random bytes
    /// Authentication succeeded. Carries the settings the client must run
    /// with; it sends no ticks until this arrives.
    #[serde(rename = "welcome")]
    Welcome { settings: ClientSettings },
    #[serde(rename = "auth_failed")]
    AuthFailed { reason: AuthFailedReason },
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

/// UTC timestamp for log lines, e.g. "2026-09-29T23:31:05Z". Shared here so
/// both binaries get identical, correlatable log timestamps for free.
pub fn ts() -> String {
    utc_timestamp(SystemTime::now(), false)
}

/// RFC 3339 UTC, e.g. "2026-09-29T23:31:05Z", or with `millis`
/// "2026-09-29T23:31:05.412Z". Hand-rolled (Howard Hinnant's
/// civil_from_days algorithm) rather than pulling in a date/time crate.
pub fn utc_timestamp(time: SystemTime, millis: bool) -> String {
    let now = time.duration_since(UNIX_EPOCH).unwrap_or_default();
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

    if millis {
        let ms = now.subsec_millis();
        format!("{year:04}-{month:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{ms:03}Z")
    } else {
        format!("{year:04}-{month:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_metrics() -> Metrics {
        Metrics {
            sample_age_ms: 120,
            cpu_percent: 37.5,
            ram_total_bytes: 32 << 30,
            ram_available_bytes: 20 << 30,
            swap_total_bytes: Some(8 << 30),
            swap_used_bytes: Some(0),
            gpus: vec![GpuMetrics {
                name: "NVIDIA GeForce RTX 3070".into(),
                utilisation_percent: 63.0,
                vram_used_bytes: 6 << 30,
                vram_total_bytes: 8 << 30,
                power_watts: None,
            }],
        }
    }

    #[test]
    fn tick_without_metrics_has_its_wire_form() {
        let tick = ClientMessage::Tick(Tick { seq: 7, metrics: None });
        let json = serde_json::to_string(&tick).unwrap();
        assert_eq!(json, r#"{"type":"tick","seq":7,"metrics":null}"#);
        let back: ClientMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ClientMessage::Tick(Tick { seq: 7, metrics: None })));
    }

    #[test]
    fn tick_with_metrics_round_trips() {
        let tick = Tick {
            seq: 0,
            metrics: Some(sample_metrics()),
        };
        let json = serde_json::to_string(&ClientMessage::Tick(tick.clone())).unwrap();
        assert!(json.starts_with(r#"{"type":"tick","seq":0,"metrics":{"sample_age_ms":120,"#));
        let ClientMessage::Tick(back) = serde_json::from_str(&json).unwrap() else {
            panic!("not a tick")
        };
        assert_eq!(back, tick);
    }

    #[test]
    fn metrics_parse_without_optional_fields() {
        // What a sender that never fills in an optional field looks like,
        // e.g. once system power is added as a new Option.
        let msg: ClientMessage = serde_json::from_str(
            r#"{"type":"tick","seq":1,"metrics":{"sample_age_ms":0,"cpu_percent":1.5,
                "ram_total_bytes":10,"ram_available_bytes":5,"gpus":[
                {"name":"g","utilisation_percent":0,"vram_used_bytes":0,"vram_total_bytes":1}]}}"#,
        )
        .unwrap();
        let ClientMessage::Tick(Tick {
            metrics: Some(metrics), ..
        }) = msg
        else {
            panic!("expected a tick with metrics")
        };
        assert_eq!(metrics.swap_total_bytes, None);
        assert_eq!(metrics.gpus[0].power_watts, None);
    }

    #[test]
    fn welcome_carries_settings() {
        let json = serde_json::to_string(&ServerMessage::Welcome {
            settings: ClientSettings::default(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"welcome","settings":{"tick_interval_ms":1000,"sampler_interval_ms":500,"sample_max_age_ms":3000,"server_silence_timeout_ms":6000,"reconnect_backoff_initial_ms":1000,"reconnect_backoff_max_ms":30000}}"#
        );
        let ServerMessage::Welcome { settings } = serde_json::from_str(&json).unwrap() else {
            panic!("not welcome")
        };
        assert_eq!(settings, ClientSettings::default());
    }

    #[test]
    fn sanitized_settings_never_hold_zero_or_inverted_values() {
        let s = ClientSettings {
            tick_interval_ms: 0,
            sampler_interval_ms: 10,
            sample_max_age_ms: 0,
            server_silence_timeout_ms: 0,
            reconnect_backoff_initial_ms: 5_000,
            reconnect_backoff_max_ms: 1_000,
        }
        .sanitized();
        assert_eq!(s.tick_interval_ms, 1);
        assert_eq!(s.sampler_interval_ms, telemetry::MIN_SAMPLER_INTERVAL_MS);
        assert_eq!(s.reconnect_backoff_max_ms, 5_000);
        assert_eq!(ClientSettings::default().sanitized(), ClientSettings::default());
    }

    #[test]
    fn timestamps_format_with_and_without_millis() {
        let t = UNIX_EPOCH + std::time::Duration::from_millis(1_793_966_468_412);
        assert_eq!(utc_timestamp(t, false), "2026-11-06T12:01:08Z");
        assert_eq!(utc_timestamp(t, true), "2026-11-06T12:01:08.412Z");
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
