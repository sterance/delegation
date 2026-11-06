//! NVIDIA GPUs through NVML, loaded at runtime (`libnvidia-ml.so.1` /
//! `nvml.dll`). On machines without the driver, `Nvml::init` fails and
//! this returns nothing.

use nvml_wrapper::enum_wrappers::device::Clock;
use nvml_wrapper::Nvml;
use protocol::hardware::Gpu;

pub struct NvidiaGpu {
    pub gpu: Gpu,
    /// Lowercase "0000:01:00.0", the same form as sysfs PCI slot names.
    /// Linux matches these against DRM cards; Windows has no use for it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub pci_slot: Option<String>,
}

pub fn gpus() -> Vec<NvidiaGpu> {
    let Ok(nvml) = Nvml::init() else { return Vec::new() };
    let count = nvml.device_count().unwrap_or(0);
    (0..count)
        .filter_map(|i| nvml.device_by_index(i).ok())
        .map(|device| NvidiaGpu {
            gpu: Gpu {
                model: device.name().ok(),
                vendor: Some("NVIDIA".to_string()),
                vram_bytes: device.memory_info().ok().map(|m| m.total),
                vram_shared: false,
                max_core_mhz: device.max_clock_info(Clock::Graphics).ok().filter(|&mhz| mhz > 0),
            },
            pci_slot: device.pci_info().ok().map(|p| normalize_bus_id(&p.bus_id)),
        })
        .collect()
}

/// NVML reports "00000000:01:00.0" (8-digit domain); sysfs uses 4 digits.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn normalize_bus_id(bus_id: &str) -> String {
    let lower = bus_id.trim().to_lowercase();
    match lower.split_once(':') {
        Some((domain, rest)) if domain.len() > 4 => format!("{}:{rest}", &domain[domain.len() - 4..]),
        _ => lower,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bus_ids_match_sysfs_form() {
        assert_eq!(super::normalize_bus_id("00000000:01:00.0"), "0000:01:00.0");
        assert_eq!(super::normalize_bus_id("0000:0A:00.0"), "0000:0a:00.0");
    }
}
