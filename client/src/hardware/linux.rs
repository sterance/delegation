//! Linux collectors: sysfs, procfs and the SMBIOS table.
//!
//! Every path goes through `HostRoot`, which is `/` natively. The Docker
//! client sets `DELEGATION_HOST_ROOT=/hostfs` with the host's `/` mounted
//! there read-only, so it reports the host's disks and OS rather than the
//! container's (see client/docker-compose.yml).

use super::nvidia::{self, NvidiaGpu};
use protocol::hardware::{Disk, DiskInterface, DiskMedia, Gpu, HardwareSpecs};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

struct HostRoot(PathBuf);

impl HostRoot {
    fn from_env() -> Self {
        HostRoot(
            std::env::var_os("DELEGATION_HOST_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/".into()),
        )
    }

    fn is_host_mount(&self) -> bool {
        self.0 != Path::new("/")
    }

    fn path(&self, p: impl AsRef<Path>) -> PathBuf {
        let p = p.as_ref();
        self.0.join(p.strip_prefix("/").unwrap_or(p))
    }

    /// The mount table of the host, not of whatever namespace we're in.
    fn mountinfo(&self) -> PathBuf {
        if self.is_host_mount() {
            self.path("/proc/1/mountinfo")
        } else {
            "/proc/self/mountinfo".into()
        }
    }
}

pub fn fill(specs: &mut HardwareSpecs) {
    let root = HostRoot::from_env();
    if let Some(cpu) = specs.cpu.as_mut() {
        cpu.max_mhz = cpu_max_mhz(&root);
    }
    // Only readable as root; otherwise RAM type and speed stay unknown.
    let dmi = std::env::var_os("DELEGATION_DMI_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.path("/sys/firmware/dmi/tables/DMI"));
    if let Ok(table) = fs::read(dmi) {
        super::apply_smbios(specs, &table);
    }
    specs.gpus = gpus(&root, nvidia::gpus(), lspci_name);
    specs.disks = disks(&root);
}

/// PRETTY_NAME from the (host's) os-release, e.g. "Debian GNU/Linux 13 (trixie)".
pub fn os_name() -> Option<String> {
    let root = HostRoot::from_env();
    let text = fs::read_to_string(root.path("/etc/os-release"))
        .or_else(|_| fs::read_to_string(root.path("/usr/lib/os-release")))
        .ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

// ---------------- CPU ----------------

/// Highest `cpuinfo_max_freq` over all cores (hybrid CPUs differ per core),
/// falling back to the highest current "cpu MHz" in /proc/cpuinfo, e.g. in
/// VMs without cpufreq.
fn cpu_max_mhz(root: &HostRoot) -> Option<u32> {
    let from_cpufreq = fs::read_dir(root.path("/sys/devices/system/cpu"))
        .ok()?
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .strip_prefix("cpu")
                .is_some_and(is_number)
        })
        .filter_map(|e| read_u64(&e.path().join("cpufreq/cpuinfo_max_freq")))
        .max()
        .map(|khz| (khz / 1000) as u32);
    from_cpufreq.or_else(|| parse_cpuinfo_mhz(&fs::read_to_string(root.path("/proc/cpuinfo")).ok()?))
}

fn parse_cpuinfo_mhz(cpuinfo: &str) -> Option<u32> {
    cpuinfo
        .lines()
        .filter(|l| l.starts_with("cpu MHz"))
        .filter_map(|l| l.split_once(':')?.1.trim().parse::<f64>().ok())
        .map(|mhz| mhz.round() as u32)
        .max()
}

// ---------------- GPUs ----------------

/// NVML's GPUs first (it knows the real model, VRAM and clocks), then any
/// other DRM card. Cards NVML already reported are matched by PCI slot.
fn gpus(root: &HostRoot, nvidia: Vec<NvidiaGpu>, name_for_slot: impl Fn(&str) -> Option<String>) -> Vec<Gpu> {
    let nvidia_slots: HashSet<String> = nvidia.iter().filter_map(|g| g.pci_slot.clone()).collect();
    let mut out: Vec<Gpu> = nvidia.into_iter().map(|g| g.gpu).collect();

    let Ok(entries) = fs::read_dir(root.path("/sys/class/drm")) else {
        return out;
    };
    let mut cards: Vec<_> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .strip_prefix("card")
                .is_some_and(is_number)
        })
        .map(|e| e.path())
        .collect();
    cards.sort();

    for card in cards {
        let device = card.join("device");
        let Some(vendor_id) = read_hex(&device.join("vendor")) else {
            continue;
        }; // not a PCI device
        let Some(slot) = fs::canonicalize(&device)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
        else {
            continue;
        };
        if nvidia_slots.contains(&slot) {
            continue;
        }
        let device_id = read_hex(&device.join("device")).unwrap_or(0);
        let vendor = vendor_name(vendor_id);
        let intel = vendor_id == 0x8086;
        out.push(Gpu {
            model: name_for_slot(&slot)
                .or_else(|| Some(format!("{} [{vendor_id:04x}:{device_id:04x}]", vendor.unwrap_or("GPU")))),
            vendor: vendor.map(str::to_string),
            // amdgpu only; Intel integrated graphics share system RAM.
            vram_bytes: if intel {
                None
            } else {
                read_u64(&device.join("mem_info_vram_total"))
            },
            vram_shared: intel,
            max_core_mhz: fs::read_to_string(device.join("pp_dpm_sclk"))
                .ok()
                .and_then(|t| max_dpm_mhz(&t))
                .or_else(|| read_u64(&card.join("gt_max_freq_mhz")).map(|m| m as u32)),
        });
    }
    out
}

fn vendor_name(id: u64) -> Option<&'static str> {
    Some(match id {
        0x1002 => "AMD",
        0x10de => "NVIDIA",
        0x8086 => "Intel",
        0x1af4 => "Virtio",
        0x15ad => "VMware",
        0x1234 => "QEMU",
        _ => return None,
    })
}

/// amdgpu's `pp_dpm_sclk`: "0: 500Mhz\n1: 2482Mhz *\n".
fn max_dpm_mhz(text: &str) -> Option<u32> {
    text.lines()
        .filter_map(|l| {
            let value = l.split_once(':')?.1.trim();
            let digits: String = value.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        })
        .max()
}

/// Marketing name from `lspci -vmm`, preferring the bracketed part:
/// "Navi 31 [Radeon RX 7900 XT/7900 XTX]" -> "Radeon RX 7900 XT/7900 XTX".
fn lspci_name(slot: &str) -> Option<String> {
    let out = std::process::Command::new("lspci")
        .args(["-vmm", "-s", slot])
        .output()
        .ok()?;
    parse_lspci_device(&String::from_utf8_lossy(&out.stdout))
}

fn parse_lspci_device(text: &str) -> Option<String> {
    let device = text.lines().find_map(|l| l.strip_prefix("Device:"))?.trim();
    let name = match (device.find('['), device.rfind(']')) {
        (Some(start), Some(end)) if end > start + 1 => &device[start + 1..end],
        _ => device,
    };
    Some(name.trim().to_string()).filter(|s| !s.is_empty())
}

// ---------------- disks ----------------

const SKIPPED_BLOCK_PREFIXES: &[&str] = &["loop", "ram", "zram", "dm-", "md", "nbd", "fd", "sr"];

fn disks(root: &HostRoot) -> Vec<Disk> {
    let Ok(entries) = fs::read_dir(root.path("/sys/block")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !SKIPPED_BLOCK_PREFIXES.iter().any(|p| n.starts_with(p)))
        .collect();
    names.sort();
    let free = free_space_by_disk(root);

    names
        .into_iter()
        .filter_map(|name| {
            let dir = root.path("/sys/block").join(&name);
            let total = read_u64(&dir.join("size"))? * 512;
            if total == 0 {
                return None; // empty card reader and the like
            }
            let resolved = fs::canonicalize(&dir).unwrap_or(dir.clone());
            let interface = classify_interface(&resolved.to_string_lossy());
            let media = match (interface, read_u64(&dir.join("queue/rotational"))) {
                (DiskInterface::Virtual, _) => DiskMedia::Unknown,
                (_, Some(1)) => DiskMedia::Hdd,
                (_, Some(0)) => DiskMedia::Ssd,
                _ => DiskMedia::Unknown,
            };
            Some(Disk {
                model: read_trimmed(&dir.join("device/model")),
                media,
                interface,
                total_bytes: Some(total),
                available_bytes: free.get(&name).copied(),
            })
        })
        .collect()
}

/// From the device's resolved sysfs path, e.g.
/// /sys/devices/pci0000:00/0000:00:06.0/0000:02:00.0/nvme/nvme0/nvme0n1.
fn classify_interface(sysfs_path: &str) -> DiskInterface {
    let p = sysfs_path.to_lowercase();
    if p.contains("/nvme") {
        DiskInterface::Nvme
    } else if p.contains("/usb") {
        DiskInterface::Usb
    } else if p.contains("virtio") || p.contains("/xen") || p.contains("vmbus") {
        DiskInterface::Virtual
    } else if p.contains("/ata") {
        DiskInterface::Sata
    } else if p.contains("end_device") || p.contains("/sas") {
        DiskInterface::Sas
    } else {
        DiskInterface::Other
    }
}

/// Free space per physical disk name ("nvme0n1"), summing `statvfs` over
/// every real mounted filesystem on it and counting each filesystem once
/// however many times it's mounted (bind mounts, btrfs subvolumes).
fn free_space_by_disk(root: &HostRoot) -> HashMap<String, u64> {
    let mut free = HashMap::new();
    let Ok(text) = fs::read_to_string(root.mountinfo()) else {
        return free;
    };
    let mut seen = HashSet::new();
    for m in text.lines().filter_map(parse_mountinfo_line) {
        // Device-backed filesystems only. Major 0 is an anonymous device:
        // tmpfs, proc, overlay... but also btrfs, so let a /dev source through.
        let block = if m.major != 0 {
            if !seen.insert(format!("{}:{}", m.major, m.minor)) {
                continue;
            }
            fs::canonicalize(root.path(format!("/sys/dev/block/{}:{}", m.major, m.minor)))
        } else if m.source.starts_with("/dev/") {
            if !seen.insert(m.source.clone()) {
                continue;
            }
            fs::canonicalize(root.path(&m.source))
        } else {
            continue;
        };
        let Some(block) = block
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        else {
            continue;
        };
        // A filesystem spanning several disks (LVM, RAID) is credited to the
        // first one rather than counted on each.
        let Some(disk) = parent_disks(root, &block, 0).into_iter().next() else {
            continue;
        };
        if let Some(bytes) = available_bytes(&root.path(&m.mount_point)) {
            *free.entry(disk).or_insert(0) += bytes;
        }
    }
    free
}

/// The whole disk(s) under a block device: a partition's parent, or the
/// disks beneath a device-mapper/md device (following `slaves/`).
fn parent_disks(root: &HostRoot, block: &str, depth: u32) -> Vec<String> {
    let dir = root.path("/sys/class/block").join(block);
    if dir.join("partition").exists() {
        return fs::canonicalize(&dir)
            .ok()
            .and_then(|p| p.parent()?.file_name().map(|n| n.to_string_lossy().into_owned()))
            .into_iter()
            .collect();
    }
    let mut slaves: Vec<String> = fs::read_dir(dir.join("slaves"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    if slaves.is_empty() || depth > 8 {
        return vec![block.to_string()];
    }
    slaves.sort();
    slaves.iter().flat_map(|s| parent_disks(root, s, depth + 1)).collect()
}

struct Mount {
    major: u32,
    minor: u32,
    mount_point: String,
    source: String,
}

/// "36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw"
fn parse_mountinfo_line(line: &str) -> Option<Mount> {
    let (before, after) = line.split_once(" - ")?;
    let fields: Vec<&str> = before.split(' ').collect();
    let (major, minor) = fields.get(2)?.split_once(':')?;
    let mut after = after.split(' ');
    let _fstype = after.next()?;
    Some(Mount {
        major: major.parse().ok()?,
        minor: minor.parse().ok()?,
        mount_point: unescape_octal(fields.get(4)?),
        source: unescape_octal(after.next()?),
    })
}

/// mountinfo escapes space, tab, newline and backslash as \ooo.
fn unescape_octal(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|b| (b'0'..=b'7').contains(b)));
        if let (b'\\', Some(digits)) = (bytes[i], octal) {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(digits).unwrap_or(""), 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn available_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: c_path is a valid NUL-terminated string and st is a properly
    // sized, writable statvfs that the call fills in.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // The fields are narrower than u64 on 32-bit targets (e.g. armv7).
    #[allow(clippy::unnecessary_cast)]
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

// ---------------- helpers ----------------

fn is_number(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn read_trimmed(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    Some(s.trim().to_string()).filter(|s| !s.is_empty())
}

fn read_u64(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

fn read_hex(path: &Path) -> Option<u64> {
    let s = read_trimmed(path)?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A throwaway directory standing in for a host root.
    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Tree {
            let dir = std::env::temp_dir().join(format!("delegation-hw-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Tree(dir)
        }
        fn file(&self, rel: &str, contents: &str) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, contents).unwrap();
        }
        fn link(&self, rel: &str, target: &str) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            symlink(target, p).unwrap();
        }
        fn root(&self) -> HostRoot {
            HostRoot(self.0.clone())
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const NVME: &str = "sys/devices/pci0000:00/0000:00:06.0/0000:02:00.0/nvme/nvme0";
    const SATA: &str = "sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda";

    fn disk_tree(name: &str) -> Tree {
        let t = Tree::new(name);
        t.file(&format!("{NVME}/model"), "WD PC SN740 SDDPMQD-512G-1101          \n");
        t.file(&format!("{NVME}/nvme0n1/size"), "1000215216\n");
        t.file(&format!("{NVME}/nvme0n1/queue/rotational"), "0\n");
        t.link(&format!("{NVME}/nvme0n1/device"), "..");
        t.file(&format!("{NVME}/nvme0n1/nvme0n1p2/partition"), "2\n");
        t.link("sys/block/nvme0n1", &format!("../../{NVME}/nvme0n1"));
        t.link(
            "sys/class/block/nvme0n1p2",
            &format!("../../../{NVME}/nvme0n1/nvme0n1p2"),
        );
        t.link("sys/dev/block/259:2", &format!("../../../{NVME}/nvme0n1/nvme0n1p2"));

        t.file(&format!("{SATA}/size"), "7814037168\n");
        t.file(&format!("{SATA}/queue/rotational"), "1\n");
        t.file(
            "sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/model",
            "ST4000DM004-2CV1\n",
        );
        t.link(&format!("{SATA}/device"), "../..");
        t.link("sys/block/sda", &format!("../../{SATA}"));

        t.file("sys/devices/virtual/block/loop0/size", "1000\n");
        t.link("sys/block/loop0", "../../sys/devices/virtual/block/loop0");

        // Root on nvme0n1p2, bind-mounted again (must count once); a loop
        // device and a tmpfs that must be ignored.
        t.file(
            "proc/1/mountinfo",
            "26 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n\
             27 26 259:2 /home /srv/my\\040home rw shared:2 - ext4 /dev/nvme0n1p2 rw\n\
             28 26 7:0 / /snap/core rw shared:3 - squashfs /dev/loop0 ro\n\
             29 26 0:25 / /tmp rw shared:4 - tmpfs tmpfs rw\n",
        );
        t.file("srv/my home/.keep", "");
        t
    }

    #[test]
    fn finds_physical_disks_and_their_free_space() {
        let t = disk_tree("disks");
        let disks = disks(&t.root());
        assert_eq!(disks.len(), 2, "{disks:?}");

        let nvme = &disks[0];
        assert_eq!(nvme.model.as_deref(), Some("WD PC SN740 SDDPMQD-512G-1101"));
        assert_eq!(nvme.interface, DiskInterface::Nvme);
        assert_eq!(nvme.media, DiskMedia::Ssd);
        assert_eq!(nvme.total_bytes, Some(1000215216 * 512));
        // Same filesystem as the temp dir; allow for other processes writing
        // to it meanwhile. Counting the bind mount twice would double it.
        let expected_free = available_bytes(&t.0).unwrap() as i64;
        let got = nvme.available_bytes.unwrap() as i64;
        assert!((got - expected_free).abs() < 256 << 20, "{got} vs {expected_free}");

        let sata = &disks[1];
        assert_eq!(sata.model.as_deref(), Some("ST4000DM004-2CV1"));
        assert_eq!(sata.interface, DiskInterface::Sata);
        assert_eq!(sata.media, DiskMedia::Hdd);
        assert_eq!(sata.available_bytes, None);
    }

    #[test]
    fn device_mapper_resolves_to_the_disk_underneath() {
        let t = disk_tree("dm");
        t.file("sys/devices/virtual/block/dm-0/dm/name", "cryptroot\n");
        t.link(
            "sys/devices/virtual/block/dm-0/slaves/nvme0n1p2",
            &format!("../../../../../../{NVME}/nvme0n1/nvme0n1p2"),
        );
        t.link("sys/class/block/dm-0", "../../../sys/devices/virtual/block/dm-0");
        assert_eq!(parent_disks(&t.root(), "dm-0", 0), vec!["nvme0n1".to_string()]);
    }

    #[test]
    fn interfaces_from_sysfs_paths() {
        let cases = [
            ("/sys/devices/pci0000:00/0000:00:06.0/0000:02:00.0/nvme/nvme0/nvme0n1", DiskInterface::Nvme),
            ("/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda", DiskInterface::Sata),
            ("/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host4/target4:0:0/4:0:0:0/block/sdb", DiskInterface::Usb),
            ("/sys/devices/pci0000:00/0000:00:04.0/virtio1/block/vda", DiskInterface::Virtual),
            ("/sys/devices/pci0000:00/0000:00:01.0/0000:01:00.0/host0/port-0:0/end_device-0:0/target0:0:0/0:0:0:0/block/sdc", DiskInterface::Sas),
            ("/sys/devices/platform/soc/mmc0/mmc0:0001/block/mmcblk0", DiskInterface::Other),
        ];
        for (path, expected) in cases {
            assert_eq!(classify_interface(path), expected, "{path}");
        }
    }

    #[test]
    fn gpus_from_drm_with_nvml_dedup() {
        let t = Tree::new("gpus");
        let intel = "sys/devices/pci0000:00/0000:00:02.0";
        t.file(&format!("{intel}/vendor"), "0x8086\n");
        t.file(&format!("{intel}/device"), "0x46a6\n");
        t.file(&format!("{intel}/drm/card0/gt_max_freq_mhz"), "1400\n");
        t.link(&format!("{intel}/drm/card0/device"), "../..");
        t.link("sys/class/drm/card0", &format!("../../../{intel}/drm/card0"));
        t.link("sys/class/drm/card0-eDP-1", &format!("../../../{intel}/drm/card0"));

        let amd = "sys/devices/pci0000:00/0000:00:01.0/0000:03:00.0";
        t.file(&format!("{amd}/vendor"), "0x1002\n");
        t.file(&format!("{amd}/device"), "0x744c\n");
        t.file(&format!("{amd}/mem_info_vram_total"), "25753026560\n");
        t.file(&format!("{amd}/pp_dpm_sclk"), "0: 500Mhz\n1: 1000Mhz\n2: 2482Mhz *\n");
        t.link(&format!("{amd}/drm/card1/device"), "../..");
        t.link("sys/class/drm/card1", &format!("../../../{amd}/drm/card1"));

        let nv = "sys/devices/pci0000:00/0000:00:01.1/0000:01:00.0";
        t.file(&format!("{nv}/vendor"), "0x10de\n");
        t.link(&format!("{nv}/drm/card2/device"), "../..");
        t.link("sys/class/drm/card2", &format!("../../../{nv}/drm/card2"));
        let nvml = vec![NvidiaGpu {
            gpu: Gpu {
                model: Some("NVIDIA GeForce RTX 4090".into()),
                ..Gpu::default()
            },
            pci_slot: Some("0000:01:00.0".into()),
        }];

        let gpus = gpus(&t.root(), nvml, |slot| {
            (slot == "0000:00:02.0").then(|| "Iris Xe Graphics".to_string())
        });
        assert_eq!(gpus.len(), 3, "{gpus:?}");
        assert_eq!(gpus[0].model.as_deref(), Some("NVIDIA GeForce RTX 4090"));
        assert_eq!(
            gpus[1],
            Gpu {
                model: Some("Iris Xe Graphics".into()),
                vendor: Some("Intel".into()),
                vram_bytes: None,
                vram_shared: true,
                max_core_mhz: Some(1400),
            }
        );
        assert_eq!(
            gpus[2],
            Gpu {
                model: Some("AMD [1002:744c]".into()),
                vendor: Some("AMD".into()),
                vram_bytes: Some(25753026560),
                vram_shared: false,
                max_core_mhz: Some(2482),
            }
        );
    }

    #[test]
    fn parses_text_sources() {
        let cpuinfo = "processor\t: 0\ncpu MHz\t\t: 400.000\n\nprocessor\t: 1\ncpu MHz\t\t: 4699.512\n";
        assert_eq!(parse_cpuinfo_mhz(cpuinfo), Some(4700));
        assert_eq!(parse_cpuinfo_mhz("processor : 0\n"), None);

        let lspci =
            "Slot:\t03:00.0\nClass:\tVGA compatible controller\nVendor:\tAdvanced Micro Devices, Inc. [AMD/ATI]\n\
                     Device:\tNavi 31 [Radeon RX 7900 XT/7900 XTX]\nSVendor:\tSapphire\nSDevice:\tNitro+ [ab]\n";
        assert_eq!(parse_lspci_device(lspci).as_deref(), Some("Radeon RX 7900 XT/7900 XTX"));
        assert_eq!(
            parse_lspci_device("Device:\tHD Graphics 620\n").as_deref(),
            Some("HD Graphics 620")
        );
        assert_eq!(parse_lspci_device(""), None);

        assert_eq!(unescape_octal("/srv/my\\040home"), "/srv/my home");
        assert_eq!(unescape_octal("/a\\134b\\"), "/a\\b\\");
    }
}
