//! Hardware specs reported to the server in the `info` message.
//!
//! Collection runs once per process on a background thread, because some
//! sources (WMI, lspci, NVML) are slow. Every collector fails on its own,
//! leaving its field empty, and the whole scan is time-limited, so a hung
//! source can never stop the client from reporting.

use log::{info, warn};
use protocol::hardware::{Cpu, HardwareSpecs, Memory};
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

#[cfg(target_os = "linux")]
mod linux;
mod nvidia;
pub mod smbios;
#[cfg(windows)]
mod windows;

#[derive(Debug, Clone, Default, Serialize)]
pub struct MachineInfo {
    pub os: String,
    pub specs: HardwareSpecs,
}

/// How long the scan may take before the client reports without it.
const COLLECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Starts `collect()` on its own thread. The receiver holds `None` until it
/// finishes, or until `COLLECT_TIMEOUT` passes, in which case it holds an
/// empty report so the client still sends its hostname and version.
pub fn spawn_collect() -> watch::Receiver<Option<Arc<MachineInfo>>> {
    let (tx, rx) = watch::channel(None);
    std::thread::spawn(move || {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(collect());
        });
        let machine = match done_rx.recv_timeout(COLLECT_TIMEOUT) {
            Ok(machine) => {
                info!(target: "hardware", "{}", summary(&machine));
                machine
            }
            Err(_) => {
                warn!(target: "hardware", "hardware scan took longer than {COLLECT_TIMEOUT:?}, reporting without it");
                MachineInfo {
                    os: os_name(),
                    specs: HardwareSpecs::default(),
                }
            }
        };
        let _ = tx.send(Some(Arc::new(machine)));
    });
    rx
}

/// Runs every collector. Slow; call from a background thread.
pub fn collect() -> MachineInfo {
    let mut specs = HardwareSpecs::default();

    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    sys.refresh_memory();
    specs.cpu = Some(Cpu {
        model: sys
            .cpus()
            .first()
            .map(|c| c.brand().trim().to_string())
            .filter(|s| !s.is_empty()),
        max_mhz: None,
        physical_cores: sysinfo::System::physical_core_count().map(|n| n as u32),
        logical_cores: Some(sys.cpus().len() as u32).filter(|&n| n > 0),
    });
    specs.memory = Some(Memory {
        total_bytes: Some(sys.total_memory()).filter(|&b| b > 0),
        ..Memory::default()
    });

    #[cfg(target_os = "linux")]
    linux::fill(&mut specs);
    #[cfg(windows)]
    windows::fill(&mut specs);

    MachineInfo { os: os_name(), specs }
}

/// Fills in RAM generation, speed and module count from an SMBIOS table.
fn apply_smbios(specs: &mut HardwareSpecs, table: &[u8]) {
    let modules = smbios::memory_modules(table);
    if modules.module_count == 0 {
        return;
    }
    let memory = specs.memory.get_or_insert_with(Memory::default);
    memory.standard = modules.standard;
    memory.speed_mts = modules.speed_mts;
    memory.module_count = Some(modules.module_count);
}

fn os_name() -> String {
    #[cfg(target_os = "linux")]
    if let Some(name) = linux::os_name() {
        return name;
    }
    sysinfo::System::long_os_version().unwrap_or_else(|| std::env::consts::OS.to_string())
}

/// One log line, e.g. "Linux Debian 13: 16 logical CPUs, 2 GPU(s), 1 disk(s)".
fn summary(machine: &MachineInfo) -> String {
    let s = &machine.specs;
    format!(
        "{}: {}, {} logical CPUs, RAM {}, {} GPU(s), {} disk(s)",
        machine.os,
        s.cpu.as_ref().and_then(|c| c.model.as_deref()).unwrap_or("unknown CPU"),
        s.cpu.as_ref().and_then(|c| c.logical_cores).unwrap_or(0),
        s.memory
            .as_ref()
            .and_then(|m| m.standard.as_deref())
            .unwrap_or("type unknown"),
        s.gpus.len(),
        s.disks.len()
    )
}
