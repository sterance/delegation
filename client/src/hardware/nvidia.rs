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
    let driver_version = nvml.sys_driver_version().ok();
    let cuda_driver_version = nvml.sys_cuda_driver_version().ok().map(cuda_version);
    (0..count)
        .filter_map(|i| nvml.device_by_index(i).ok())
        .map(|device| NvidiaGpu {
            gpu: Gpu {
                model: device.name().ok(),
                vendor: Some("NVIDIA".to_string()),
                vram_bytes: device.memory_info().ok().map(|m| m.total),
                vram_shared: false,
                max_core_mhz: device.max_clock_info(Clock::Graphics).ok().filter(|&mhz| mhz > 0),
                driver_version: driver_version.clone(),
                cuda_driver_version: cuda_driver_version.clone(),
                compute_capability: device
                    .cuda_compute_capability()
                    .ok()
                    .map(|c| format!("{}.{}", c.major, c.minor)),
            },
            pci_slot: device.pci_info().ok().map(|p| normalize_bus_id(&p.bus_id)),
        })
        .collect()
}

/// NVML's CUDA driver version, e.g. 12080, as "12.8".
fn cuda_version(v: i32) -> String {
    format!(
        "{}.{}",
        nvml_wrapper::cuda_driver_version_major(v),
        nvml_wrapper::cuda_driver_version_minor(v)
    )
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

    #[test]
    fn cuda_versions_read_like_nvidia_smi() {
        assert_eq!(super::cuda_version(12080), "12.8");
        assert_eq!(super::cuda_version(13040), "13.4");
    }
}
