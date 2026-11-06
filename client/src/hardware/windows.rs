//! Windows collectors: registry, SMBIOS via GetSystemFirmwareTable, DXGI
//! and the Storage WMI namespace. None of these need admin rights.

use super::nvidia;
use log::info;
use protocol::hardware::{Disk, DiskInterface, DiskMedia, Gpu, HardwareSpecs};
use serde::Deserialize;
use std::collections::HashMap;
use windows::core::{w, HSTRING};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
use windows::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};

pub fn fill(specs: &mut HardwareSpecs) {
    if let Some(cpu) = specs.cpu.as_mut() {
        cpu.max_mhz = cpu_mhz();
    }
    if let Some(raw) = smbios_table() {
        super::apply_smbios(specs, super::smbios::strip_windows_header(&raw));
    }
    specs.gpus = gpus();
    match disks() {
        Ok(disks) => specs.disks = disks,
        Err(e) => info!(target: "hardware", "disk query failed: {e}"),
    }
}

/// The processor's rated speed as Windows records it at boot.
fn cpu_mhz() -> Option<u32> {
    let mut mhz: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: the out pointers are valid for a DWORD for the whole call.
    let err = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0"),
            w!("~MHz"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut mhz as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    (err.is_ok() && mhz > 0).then_some(mhz)
}

/// The raw SMBIOS table, including Windows' 8-byte header.
fn smbios_table() -> Option<Vec<u8>> {
    // SAFETY: first call only asks for the size; the second gets a buffer
    // of exactly that size.
    let size = unsafe { GetSystemFirmwareTable(RSMB, 0, None) };
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let written = unsafe { GetSystemFirmwareTable(RSMB, 0, Some(&mut buf)) };
    (written > 0 && written <= size).then(|| {
        buf.truncate(written as usize);
        buf
    })
}

/// DXGI for every adapter's name and 64-bit dedicated VRAM. (WMI's
/// `Win32_VideoController.AdapterRAM` is 32-bit and tops out at 4 GB.)
/// NVML replaces the NVIDIA entries when it's available, since it also
/// knows the clock speed.
fn gpus() -> Vec<Gpu> {
    let nvidia: Vec<Gpu> = nvidia::gpus().into_iter().map(|g| g.gpu).collect();
    let mut out = Vec::new();
    // SAFETY: plain COM calls on interfaces windows-rs manages.
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return nvidia;
    };
    let mut i = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
        i += 1;
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        // Skips the "Microsoft Basic Render Driver".
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || desc.VendorId == 0x1414 {
            continue;
        }
        if desc.VendorId == 0x10de && !nvidia.is_empty() {
            continue;
        }
        let name_len = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let dedicated = desc.DedicatedVideoMemory as u64;
        // Integrated GPUs get a small carve-out (typically 128-512 MB) and
        // otherwise use system RAM.
        let shared = dedicated < (1 << 30);
        out.push(Gpu {
            model: Some(
                String::from_utf16_lossy(&desc.Description[..name_len])
                    .trim()
                    .to_string(),
            ),
            vendor: vendor_name(desc.VendorId).map(str::to_string),
            vram_bytes: (!shared).then_some(dedicated),
            vram_shared: shared,
            max_core_mhz: None,
        });
    }
    let mut all = nvidia;
    all.extend(out);
    all
}

fn vendor_name(id: u32) -> Option<&'static str> {
    Some(match id {
        0x1002 => "AMD",
        0x10de => "NVIDIA",
        0x8086 => "Intel",
        0x5143 => "Qualcomm",
        _ => return None,
    })
}

#[derive(Deserialize)]
#[serde(rename = "MSFT_PhysicalDisk", rename_all = "PascalCase")]
struct PhysicalDisk {
    device_id: String,
    friendly_name: Option<String>,
    model: Option<String>,
    media_type: Option<u16>,
    bus_type: Option<u16>,
    size: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename = "MSFT_Partition", rename_all = "PascalCase")]
struct Partition {
    disk_number: u32,
    drive_letter: Option<DriveLetter>,
}

/// WMI hands a CIM char16 back as either a one-letter string or a number,
/// depending on the provider.
#[derive(Deserialize)]
#[serde(untagged)]
enum DriveLetter {
    Text(String),
    Code(u16),
}

impl DriveLetter {
    fn letter(&self) -> Option<char> {
        let c = match self {
            DriveLetter::Text(s) => s.chars().next()?,
            DriveLetter::Code(n) => char::from_u32(*n as u32)?,
        };
        c.is_ascii_alphabetic().then_some(c)
    }
}

fn disks() -> wmi::WMIResult<Vec<Disk>> {
    let wmi = wmi::WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage")?;
    let physical: Vec<PhysicalDisk> = wmi.query()?;
    let partitions: Vec<Partition> = wmi.query().unwrap_or_default();

    // Free space per disk number, summed over its lettered partitions.
    let mut free: HashMap<u32, u64> = HashMap::new();
    for p in &partitions {
        let Some(letter) = p.drive_letter.as_ref().and_then(DriveLetter::letter) else {
            continue;
        };
        if let Some(bytes) = free_bytes(letter) {
            *free.entry(p.disk_number).or_insert(0) += bytes;
        }
    }

    let mut disks: Vec<(u32, Disk)> = physical
        .into_iter()
        .map(|d| {
            let number = d.device_id.parse().unwrap_or(u32::MAX);
            let interface = bus_type(d.bus_type.unwrap_or(0));
            let disk = Disk {
                model: d
                    .friendly_name
                    .or(d.model)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                media: match d.media_type {
                    Some(3) => DiskMedia::Hdd,
                    Some(4) => DiskMedia::Ssd,
                    _ => DiskMedia::Unknown,
                },
                interface,
                total_bytes: d.size.filter(|&s| s > 0),
                available_bytes: free.get(&number).copied(),
            };
            (number, disk)
        })
        .collect();
    disks.sort_by_key(|(n, _)| *n);
    Ok(disks.into_iter().map(|(_, d)| d).collect())
}

/// MSFT_PhysicalDisk.BusType values.
fn bus_type(code: u16) -> DiskInterface {
    match code {
        17 => DiskInterface::Nvme,
        11 => DiskInterface::Sata,
        10 => DiskInterface::Sas,
        7 => DiskInterface::Usb,
        14 | 15 => DiskInterface::Virtual, // Virtual, File Backed Virtual
        _ => DiskInterface::Other,
    }
}

fn free_bytes(letter: char) -> Option<u64> {
    let mut available: u64 = 0;
    let root = HSTRING::from(format!("{letter}:\\"));
    // SAFETY: the out pointer is valid for a u64 for the whole call.
    unsafe { GetDiskFreeSpaceExW(&root, Some(&mut available), None, None) }.ok()?;
    Some(available)
}
