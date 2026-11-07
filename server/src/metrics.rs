//! Validation of the metrics clients send in ticks. They're untrusted
//! input: a bad sample is dropped (and logged at debug level by the
//! caller), never allowed to drop the connection or panic.

use crate::config::MetricsLimits;
use protocol::Metrics;

/// Why a sample's content was rejected. Spacing is checked separately, by
/// `live::Live::record_sample`, since it depends on the previous sample.
pub fn validate(m: &Metrics, limits: &MetricsLimits) -> Result<(), String> {
    percent("cpu_percent", m.cpu_percent)?;
    // RAM is reported as available rather than used; available > total is
    // the same impossibility as used > total.
    if m.ram_available_bytes > m.ram_total_bytes {
        return Err(format!(
            "ram_available_bytes {} exceeds ram_total_bytes {}",
            m.ram_available_bytes, m.ram_total_bytes
        ));
    }
    if let (Some(used), Some(total)) = (m.swap_used_bytes, m.swap_total_bytes) {
        used_within_total("swap", used, total)?;
    }
    if m.gpus.len() > limits.max_gpus {
        return Err(format!("{} GPUs listed, more than {}", m.gpus.len(), limits.max_gpus));
    }
    for (i, gpu) in m.gpus.iter().enumerate() {
        let len = gpu.name.chars().count();
        if len > limits.max_string_len {
            return Err(format!(
                "gpus[{i}].name is {len} characters, more than {}",
                limits.max_string_len
            ));
        }
        percent(&format!("gpus[{i}].utilisation_percent"), gpu.utilisation_percent)?;
        used_within_total(&format!("gpus[{i}].vram"), gpu.vram_used_bytes, gpu.vram_total_bytes)?;
        if let Some(watts) = gpu.power_watts {
            if !watts.is_finite() || watts < 0.0 {
                return Err(format!("gpus[{i}].power_watts is {watts}"));
            }
        }
    }
    Ok(())
}

fn percent(field: &str, value: f32) -> Result<(), String> {
    if value.is_finite() && (0.0..=100.0).contains(&value) {
        Ok(())
    } else {
        Err(format!("{field} is {value}, outside 0-100"))
    }
}

fn used_within_total(field: &str, used: u64, total: u64) -> Result<(), String> {
    if used > total {
        Err(format!("{field} used {used} exceeds total {total}"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::GpuMetrics;
    use std::time::Duration;

    const LIMITS: MetricsLimits = MetricsLimits {
        history_samples: 60,
        min_spacing: Duration::from_millis(500),
        max_gpus: 2,
        max_string_len: 8,
    };

    fn gpu() -> GpuMetrics {
        GpuMetrics {
            name: "RTX".into(),
            utilisation_percent: 50.0,
            vram_used_bytes: 1,
            vram_total_bytes: 2,
            power_watts: Some(100.0),
        }
    }

    fn good() -> Metrics {
        Metrics {
            sample_age_ms: 0,
            cpu_percent: 12.5,
            ram_total_bytes: 100,
            ram_available_bytes: 40,
            swap_total_bytes: Some(10),
            swap_used_bytes: Some(10),
            gpus: vec![gpu()],
        }
    }

    fn rejected(f: impl FnOnce(&mut Metrics)) -> String {
        let mut m = good();
        f(&mut m);
        validate(&m, &LIMITS).expect_err("expected rejection")
    }

    #[test]
    fn accepts_a_normal_sample_and_edge_values() {
        assert!(validate(&good(), &LIMITS).is_ok());
        let mut m = good();
        m.cpu_percent = 100.0;
        m.gpus[0].utilisation_percent = 0.0;
        m.gpus[0].power_watts = None;
        m.swap_total_bytes = None;
        m.swap_used_bytes = None;
        m.gpus.push(gpu());
        assert!(validate(&m, &LIMITS).is_ok());
    }

    #[test]
    fn rejects_out_of_range_percentages() {
        assert!(rejected(|m| m.cpu_percent = 100.5).contains("cpu_percent"));
        assert!(rejected(|m| m.cpu_percent = -0.1).contains("cpu_percent"));
        assert!(rejected(|m| m.gpus[0].utilisation_percent = 101.0).contains("utilisation_percent"));
    }

    #[test]
    fn rejects_nan_and_infinity() {
        assert!(rejected(|m| m.cpu_percent = f32::NAN).contains("cpu_percent"));
        assert!(rejected(|m| m.gpus[0].utilisation_percent = f32::INFINITY).contains("utilisation_percent"));
        assert!(rejected(|m| m.gpus[0].power_watts = Some(f32::NAN)).contains("power_watts"));
        assert!(rejected(|m| m.gpus[0].power_watts = Some(f32::NEG_INFINITY)).contains("power_watts"));
    }

    #[test]
    fn rejects_too_many_gpus_and_long_strings() {
        assert!(rejected(|m| m.gpus = vec![gpu(), gpu(), gpu()]).contains("3 GPUs"));
        assert!(rejected(|m| m.gpus[0].name = "123456789".into()).contains("name"));
        // Characters, not bytes: 8 two-byte characters is within the limit.
        let mut m = good();
        m.gpus[0].name = "éééééééé".into();
        assert!(validate(&m, &LIMITS).is_ok());
    }

    #[test]
    fn rejects_used_above_total() {
        assert!(rejected(|m| m.ram_available_bytes = 101).contains("ram_available_bytes"));
        assert!(rejected(|m| m.swap_used_bytes = Some(11)).contains("swap"));
        assert!(rejected(|m| m.gpus[0].vram_used_bytes = 3).contains("vram"));
    }
}
