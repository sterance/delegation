//! Which llama.cpp variant to use, from the hardware report alone: one pure
//! function from GPUs to an ordered list of candidates (best first, always
//! ending with `cpu`). The runtime tries them in order and falls back when
//! one doesn't work, so this only has to be a good first guess.

use super::runtime::PinnedVariant;
use protocol::hardware::Gpu;

/// CUDA 13 dropped Maxwell, Pascal and Volta: the oldest GPUs it supports
/// are Turing, compute capability 7.5 (CUDA 13.0 release notes).
pub const CUDA_13_MIN_COMPUTE_CAPABILITY: (u32, u32) = (7, 5);

/// "12.8" -> (12, 8); "13" -> (13, 0).
pub fn parse_major_minor(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = match parts.next() {
        Some(m) => m.parse().ok()?,
        None => 0,
    };
    Some((major, minor))
}

fn is(vendor: &Option<String>, name: &str) -> bool {
    vendor.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(name))
}

/// The automatic choice, in order, among `pinned` (this platform's
/// variants):
///
/// 1. NVIDIA GPU that NVML could read: `cuda-13` if the driver supports
///    that variant's CUDA version and every NVIDIA GPU is new enough for
///    CUDA 13; otherwise `cuda-12` if the driver supports its version;
///    then `vulkan`.
/// 2. Any other GPU with its own memory (AMD, Intel Arc, or NVIDIA without
///    NVML): `vulkan`.
/// 3. Only integrated GPUs, or none: CPU only. (They share system RAM and
///    are rarely faster; the server can still select `vulkan`.)
///
/// `cpu` always comes last.
pub fn automatic(gpus: &[Gpu], pinned: &[PinnedVariant]) -> Vec<String> {
    let requires = |name: &str| -> Option<Option<(u32, u32)>> {
        let v = pinned.iter().find(|v| v.variant == name)?;
        Some(v.requires.cuda_driver.as_deref().and_then(parse_major_minor))
    };
    let mut out: Vec<String> = Vec::new();

    let nvidia: Vec<&Gpu> = gpus
        .iter()
        .filter(|g| is(&g.vendor, "NVIDIA") && g.cuda_driver_version.is_some())
        .collect();
    if !nvidia.is_empty() {
        let driver = nvidia
            .iter()
            .filter_map(|g| g.cuda_driver_version.as_deref().and_then(parse_major_minor))
            .max();
        // Every GPU must be supported; one we can't read counts as too old.
        let oldest = nvidia
            .iter()
            .map(|g| g.compute_capability.as_deref().and_then(parse_major_minor))
            .min()
            .flatten();
        let driver_ok = |name: &str| match (requires(name), driver) {
            (Some(Some(needed)), Some(have)) => have >= needed,
            _ => false,
        };
        if driver_ok("cuda-13") && oldest.is_some_and(|cc| cc >= CUDA_13_MIN_COMPUTE_CAPABILITY) {
            out.push("cuda-13".into());
        } else if driver_ok("cuda-12") {
            out.push("cuda-12".into());
        }
    }
    let discrete = gpus.iter().any(|g| !g.vram_shared && g.vendor.is_some());
    if (discrete || !nvidia.is_empty()) && requires("vulkan").is_some() {
        out.push("vulkan".into());
    }
    out.push("cpu".into());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::runtime::Requires;

    fn pinned(cuda12: &str) -> Vec<PinnedVariant> {
        let v = |name: &str, cuda: Option<&str>| PinnedVariant {
            os: "linux".into(),
            arch: "x86_64".into(),
            variant: name.into(),
            requires: Requires {
                cuda_driver: cuda.map(str::to_string),
            },
            archives: Vec::new(),
        };
        vec![
            v("cpu", None),
            v("cuda-12", Some(cuda12)),
            v("cuda-13", Some("13.4")),
            v("vulkan", None),
        ]
    }

    fn nvidia(cuda: &str, cc: &str) -> Gpu {
        Gpu {
            model: Some("NVIDIA GeForce".into()),
            vendor: Some("NVIDIA".into()),
            vram_bytes: Some(8 << 30),
            cuda_driver_version: Some(cuda.into()),
            compute_capability: Some(cc.into()),
            driver_version: Some("580.1".into()),
            ..Gpu::default()
        }
    }

    fn other(vendor: &str, shared: bool) -> Gpu {
        Gpu {
            vendor: Some(vendor.into()),
            vram_shared: shared,
            ..Gpu::default()
        }
    }

    #[test]
    fn the_automatic_choice_follows_the_hardware() {
        let table: Vec<(&str, Vec<Gpu>, &[&str])> = vec![
            (
                "recent NVIDIA, new driver",
                vec![nvidia("13.4", "8.6")],
                &["cuda-13", "vulkan", "cpu"],
            ),
            (
                "GPU too old for CUDA 13",
                vec![nvidia("13.4", "6.1")],
                &["cuda-12", "vulkan", "cpu"],
            ),
            (
                "driver too old for CUDA 13",
                vec![nvidia("12.9", "8.9")],
                &["cuda-12", "vulkan", "cpu"],
            ),
            (
                "driver too old for either",
                vec![nvidia("12.2", "8.6")],
                &["vulkan", "cpu"],
            ),
            (
                "one old GPU among new ones",
                vec![nvidia("13.4", "8.6"), nvidia("13.4", "7.0")],
                &["cuda-12", "vulkan", "cpu"],
            ),
            ("AMD discrete", vec![other("AMD", false)], &["vulkan", "cpu"]),
            ("Intel integrated only", vec![other("Intel", true)], &["cpu"]),
            (
                "integrated Intel beside a discrete AMD",
                vec![other("Intel", true), other("AMD", false)],
                &["vulkan", "cpu"],
            ),
            ("no GPU", vec![], &["cpu"]),
        ];
        for (machine, gpus, want) in table {
            assert_eq!(automatic(&gpus, &pinned("12.8")), want, "{machine}");
        }
        // Windows's cuda-12 build is CUDA 12.4: a 12.4 driver is enough there.
        assert_eq!(
            automatic(&[nvidia("12.4", "8.6")], &pinned("12.4")),
            ["cuda-12", "vulkan", "cpu"]
        );
    }

    #[test]
    fn only_pinned_variants_are_offered() {
        // Linux arm64: no cuda-12.
        let arm: Vec<PinnedVariant> = pinned("12.8").into_iter().filter(|v| v.variant != "cuda-12").collect();
        assert_eq!(automatic(&[nvidia("12.9", "8.7")], &arm), ["vulkan", "cpu"]);
        let cpu_only: Vec<PinnedVariant> = pinned("12.8").into_iter().filter(|v| v.variant == "cpu").collect();
        assert_eq!(automatic(&[nvidia("13.4", "8.6")], &cpu_only), ["cpu"]);
    }

    #[test]
    fn versions_parse() {
        assert_eq!(parse_major_minor("12.8"), Some((12, 8)));
        assert_eq!(parse_major_minor("13"), Some((13, 0)));
        assert_eq!(parse_major_minor("7.5"), Some((7, 5)));
        assert!(parse_major_minor("12.10").unwrap() > parse_major_minor("12.9").unwrap());
        assert_eq!(parse_major_minor("x"), None);
    }
}
