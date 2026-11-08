//! The once-a-second `tick` a client sends, the resource metrics it
//! carries, and the client-behaviour settings the server pushes in
//! `welcome`.
//!
//! Everything is in raw units (bytes, percent, watts); the dashboard does
//! all formatting. New measurements (system power, for one) should be added
//! as `Option` fields: serde treats a missing `Option` as `None`, so a
//! sender that doesn't fill them in still parses.

use crate::models::InferenceSettings;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Client to server every `tick_interval_ms`. Doubles as the liveness
/// signal, so it's sent even when there's no fresh sample. Not acked.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Tick {
    /// Per-connection counter from 0; for debugging and spotting drops.
    pub seq: u64,
    /// `None` if the sampler has no reading newer than `sample_max_age_ms`.
    pub metrics: Option<Metrics>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Metrics {
    /// Age of this sample when the tick was sent, so nothing depends on the
    /// client's clock agreeing with the server's.
    pub sample_age_ms: u32,
    pub cpu_percent: f32,
    pub ram_total_bytes: u64,
    /// Memory available for new allocations (Linux `MemAvailable`), not
    /// "free", which excludes reclaimable cache.
    pub ram_available_bytes: u64,
    pub swap_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    /// Empty when the machine has no supported (NVIDIA) GPU.
    pub gpus: Vec<GpuMetrics>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct GpuMetrics {
    pub name: String,
    pub utilisation_percent: f32,
    pub vram_used_bytes: u64,
    pub vram_total_bytes: u64,
    /// `None` if the driver doesn't report power for this GPU.
    pub power_watts: Option<f32>,
}

/// The client-behaviour values from the server's `config.env`, pushed in
/// `welcome` so clients on other machines (the tray app has no config file)
/// follow one place. The client caches the last set it received for its
/// reconnect backoff, and uses `Default` before it has ever connected.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientSettings {
    pub tick_interval_ms: u64,
    pub sampler_interval_ms: u64,
    pub sample_max_age_ms: u64,
    pub server_silence_timeout_ms: u64,
    pub reconnect_backoff_initial_ms: u64,
    pub reconnect_backoff_max_ms: u64,
    /// Defaulted when missing, so a settings cache written before these
    /// existed still loads.
    #[serde(default)]
    pub inference: InferenceSettings,
}

/// The sampler can't go faster than this: CPU % is the difference between
/// two readings, and sysinfo needs them at least ~200 ms apart.
pub const MIN_SAMPLER_INTERVAL_MS: u64 = 200;

impl Default for ClientSettings {
    fn default() -> Self {
        ClientSettings {
            tick_interval_ms: 1_000,
            sampler_interval_ms: 500,
            sample_max_age_ms: 3_000,
            server_silence_timeout_ms: 6_000,
            reconnect_backoff_initial_ms: 1_000,
            reconnect_backoff_max_ms: 30_000,
            inference: InferenceSettings::default(),
        }
    }
}

impl ClientSettings {
    /// The server validates these before sending them, but a zero would
    /// panic tokio's interval timer, so the client never trusts them blindly.
    pub fn sanitized(self) -> Self {
        let initial = self.reconnect_backoff_initial_ms.max(1);
        ClientSettings {
            tick_interval_ms: self.tick_interval_ms.max(1),
            sampler_interval_ms: self.sampler_interval_ms.max(MIN_SAMPLER_INTERVAL_MS),
            sample_max_age_ms: self.sample_max_age_ms.max(1),
            server_silence_timeout_ms: self.server_silence_timeout_ms.max(1),
            reconnect_backoff_initial_ms: initial,
            reconnect_backoff_max_ms: self.reconnect_backoff_max_ms.max(initial),
            inference: self.inference.sanitized(),
        }
    }

    pub fn tick_interval(&self) -> Duration {
        Duration::from_millis(self.tick_interval_ms)
    }

    pub fn sampler_interval(&self) -> Duration {
        Duration::from_millis(self.sampler_interval_ms)
    }

    pub fn sample_max_age(&self) -> Duration {
        Duration::from_millis(self.sample_max_age_ms)
    }

    pub fn server_silence_timeout(&self) -> Duration {
        Duration::from_millis(self.server_silence_timeout_ms)
    }

    pub fn reconnect_backoff_initial(&self) -> Duration {
        Duration::from_millis(self.reconnect_backoff_initial_ms)
    }

    pub fn reconnect_backoff_max(&self) -> Duration {
        Duration::from_millis(self.reconnect_backoff_max_ms)
    }
}
