//! Hardware specs a client reports in its `info` message.
//!
//! Every field is optional and in raw units (bytes, MHz, MT/s); the
//! dashboard does all formatting. A collector that can't read something
//! leaves it `None` rather than failing the whole report, and
//! `#[serde(default)]` everywhere means older or newer clients that send
//! more or fewer fields still parse. The dashboard mirrors these types by
//! hand in `frontend/src/api.ts`.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct HardwareSpecs {
    pub cpu: Option<Cpu>,
    pub memory: Option<Memory>,
    pub gpus: Vec<Gpu>,
    pub disks: Vec<Disk>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct Cpu {
    pub model: Option<String>,
    pub max_mhz: Option<u32>,
    pub physical_cores: Option<u32>,
    pub logical_cores: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct Memory {
    pub total_bytes: Option<u64>,
    /// e.g. "DDR5", "LPDDR4". From SMBIOS, so often missing on Linux
    /// when the client isn't running as root.
    pub standard: Option<String>,
    pub speed_mts: Option<u32>,
    pub module_count: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct Gpu {
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub vram_bytes: Option<u64>,
    /// Integrated GPU using system RAM; `vram_bytes` is then not meaningful.
    pub vram_shared: bool,
    pub max_core_mhz: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct Disk {
    pub model: Option<String>,
    pub media: DiskMedia,
    pub interface: DiskInterface,
    pub total_bytes: Option<u64>,
    /// Free space summed over this disk's mounted filesystems.
    pub available_bytes: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiskMedia {
    Hdd,
    Ssd,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiskInterface {
    Nvme,
    Sata,
    Sas,
    Usb,
    Virtual,
    #[default]
    #[serde(other)]
    Other,
}
