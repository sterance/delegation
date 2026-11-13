//! Getting `llama-server`: a pinned llama.cpp release in one of several
//! variants (CPU, CUDA 12, CUDA 13, Vulkan), downloaded the first time it's
//! needed, checked against the SHA-256 recorded in `runtime-pins.json`, and
//! unpacked into `runtime/<variant>-<version>/`.
//!
//! **Executables never come from the server.** The server is trusted to
//! choose models (data), but if it could name a binary, a compromised
//! server would be code execution on every volunteer's machine. So the
//! release, its URLs and its hashes are compiled in (`include_str!` of
//! `runtime-pins.json`), and the server can only choose among the variants
//! pinned there (`select_runtime`).
//!
//! **CPU always works.** Which variant to use comes from the hardware
//! (`select.rs`) or the server; whatever it is, `cpu` is the last
//! candidate. A GPU variant has to prove it can see a GPU (`llama-server
//! --list-devices`) before it's used, and one that doesn't, or that fails
//! to load a model, is marked failed and skipped until the client restarts
//! or the server selects it again.

use super::process;
use super::select;
use super::store::{self, Download, FreeSpaceFn, PullError};
use log::{info, warn};
use protocol::hardware::Gpu;
use protocol::{Backend, LlamaCppVersion, RuntimeInfo, RuntimeState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

// ---------------- the pins: data, not code ----------------

/// The pins compiled into this client. A signed, downloaded manifest
/// (later) has the same format and goes through the same parser.
const EMBEDDED_PINS: &str = include_str!("../../runtime-pins.json");

/// Manifest formats this build understands.
pub const KNOWN_SCHEMAS: &[u32] = &[1];
/// Variants the selection logic knows about.
pub const KNOWN_VARIANTS: &[&str] = &["cpu", "cuda-12", "cuda-13", "vulkan"];
const RELEASE_URL_PREFIX: &str = "https://github.com/ggml-org/llama.cpp/releases/download/";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    TarGz,
    Zip,
}

/// One file of a variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedArchive {
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub format: ArchiveFormat,
}

/// What the selection logic needs to know about a variant.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Requires {
    /// The CUDA version the driver must support, e.g. "12.8".
    pub cuda_driver: Option<String>,
}

/// One build for one platform: one or more archives (a CUDA build and its
/// CUDA runtime) unpacked into the same directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedVariant {
    /// `std::env::consts::OS`, e.g. "linux".
    pub os: String,
    /// `std::env::consts::ARCH`, e.g. "x86_64".
    pub arch: String,
    pub variant: String,
    #[serde(default)]
    pub requires: Requires,
    pub archives: Vec<PinnedArchive>,
}

impl PinnedVariant {
    pub fn total_bytes(&self) -> u64 {
        self.archives.iter().map(|a| a.size_bytes).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pins {
    pub schema: u32,
    pub backend: Backend,
    /// e.g. "b12110".
    pub version: String,
    pub variants: Vec<PinnedVariant>,
}

/// Parses and checks a pins manifest. The weekly bump workflow relies on
/// this (through the test of the embedded file) to catch a bad edit.
pub fn parse_pins(text: &str) -> Result<Pins, String> {
    let pins: Pins = serde_json::from_str(text).map_err(|e| format!("invalid runtime pins: {e}"))?;
    validate_pins(&pins)?;
    Ok(pins)
}

pub fn validate_pins(pins: &Pins) -> Result<(), String> {
    if !KNOWN_SCHEMAS.contains(&pins.schema) {
        return Err(format!("unknown schema {}", pins.schema));
    }
    if pins.backend != Backend::LlamaCpp {
        return Err(format!("unsupported backend {:?}", pins.backend));
    }
    if LlamaCppVersion::parse(&pins.version).is_none() {
        return Err(format!("{:?} isn't a llama.cpp version (b<number>)", pins.version));
    }
    let prefix = format!("{RELEASE_URL_PREFIX}{}/", pins.version);
    let mut seen = std::collections::BTreeSet::new();
    for v in &pins.variants {
        let what = format!("{} {} {}", v.os, v.arch, v.variant);
        if !KNOWN_VARIANTS.contains(&v.variant.as_str()) {
            return Err(format!("{what}: unknown variant {:?}", v.variant));
        }
        if !seen.insert((v.os.clone(), v.arch.clone(), v.variant.clone())) {
            return Err(format!("{what}: pinned twice"));
        }
        if v.archives.is_empty() {
            return Err(format!("{what}: no archives"));
        }
        if v.variant.starts_with("cuda-")
            && v.requires
                .cuda_driver
                .as_deref()
                .and_then(select::parse_major_minor)
                .is_none()
        {
            return Err(format!(
                "{what}: requires.cuda_driver must be a CUDA version like \"12.8\""
            ));
        }
        for a in &v.archives {
            let name = a.url.strip_prefix(&prefix).unwrap_or("");
            if name.is_empty() || name.contains(['/', '?', '#', '\\']) || name.contains("..") {
                return Err(format!(
                    "{what}: {} isn't a file of the {} release ({prefix}<file>)",
                    a.url, pins.version
                ));
            }
            let format_matches = match a.format {
                ArchiveFormat::TarGz => name.ends_with(".tar.gz"),
                ArchiveFormat::Zip => name.ends_with(".zip"),
            };
            if !format_matches {
                return Err(format!("{what}: {name} isn't a {:?} archive", a.format));
            }
            if !store::is_sha256_hex(&a.sha256) {
                return Err(format!("{what}: {name}'s sha256 must be 64 lowercase hex digits"));
            }
            if a.size_bytes == 0 {
                return Err(format!("{what}: {name}'s size must not be 0"));
            }
        }
    }
    if !pins.variants.iter().all(|v| {
        pins.variants
            .iter()
            .any(|c| c.os == v.os && c.arch == v.arch && c.variant == "cpu")
    }) {
        return Err("every platform needs a cpu variant (the fallback)".into());
    }
    Ok(())
}

/// The pins this client runs with: the embedded file, for now. Everything
/// gets them from here, so a downloaded manifest only changes this.
pub fn pins() -> &'static Pins {
    static PINS: OnceLock<Pins> = OnceLock::new();
    // The unit test of the embedded file guarantees this parses.
    PINS.get_or_init(|| parse_pins(EMBEDDED_PINS).expect("the embedded runtime-pins.json is invalid"))
}

/// The pinned variants for this machine.
pub fn pinned_for_this_platform() -> Vec<PinnedVariant> {
    pins()
        .variants
        .iter()
        .filter(|v| v.os == std::env::consts::OS && v.arch == std::env::consts::ARCH)
        .cloned()
        .collect()
}

pub const SERVER_EXE: &str = if cfg!(windows) {
    "llama-server.exe"
} else {
    "llama-server"
};
/// Written last, holding the archives' hashes: the install is complete.
const MARKER: &str = ".installed";
/// The server's choice, kept across restarts.
const SELECTION_FILE: &str = "selection.json";
/// How long `--list-devices` may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a load waits for the hardware scan before choosing without it.
const HARDWARE_WAIT: Duration = Duration::from_secs(35);
/// Room for the unpacked files on top of the downloads.
const UNPACK_FACTOR: u64 = 2;

/// Which variant to use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "variant")]
pub enum Selection {
    Auto,
    Explicit(String),
}

/// A variant ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub variant: String,
    pub exe: PathBuf,
}

#[derive(Default)]
struct State {
    downloading: Option<String>,
    /// Variant -> why it doesn't work here.
    failed: BTreeMap<String, String>,
    /// Variant -> the GPUs it found. Only GPU variants that passed the check.
    devices: BTreeMap<String, Vec<String>>,
    /// What the last load used.
    active: Option<String>,
}

pub struct Runtime {
    root: PathBuf,
    version: String,
    variants: Vec<PinnedVariant>,
    free_space: FreeSpaceFn,
    /// One install (or check) at a time; a second caller waits and then
    /// finds it done.
    install: Mutex<()>,
    state: Mutex<State>,
    selection: Mutex<Selection>,
    /// None until the hardware scan is in.
    hardware: Mutex<Option<Vec<Gpu>>>,
    hardware_ready: Condvar,
    hardware_wait: Duration,
}

/// What a test can swap: where the pins come from, free space, and how
/// long to wait for the hardware.
pub(crate) struct RuntimeParts {
    pub version: String,
    pub variants: Vec<PinnedVariant>,
    pub free_space: FreeSpaceFn,
    pub hardware_wait: Duration,
}

impl Runtime {
    pub fn new(data_dir: &Path) -> Runtime {
        Self::with_parts(
            data_dir,
            RuntimeParts {
                version: pins().version.clone(),
                variants: pinned_for_this_platform(),
                free_space: Arc::new(crate::hardware::free_space),
                hardware_wait: HARDWARE_WAIT,
            },
        )
    }

    pub(crate) fn with_parts(data_dir: &Path, parts: RuntimeParts) -> Runtime {
        let root = data_dir.join("runtime");
        store::remove_stale_partials(&root, 1);
        let selection = read_selection(&root, &parts.variants);
        let runtime = Runtime {
            root,
            version: parts.version,
            variants: parts.variants,
            free_space: parts.free_space,
            install: Mutex::new(()),
            state: Mutex::new(State::default()),
            selection: Mutex::new(selection),
            hardware: Mutex::new(None),
            hardware_ready: Condvar::new(),
            hardware_wait: parts.hardware_wait,
        };
        runtime.remove_other_versions();
        runtime
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn variants(&self) -> &[PinnedVariant] {
        &self.variants
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn selection(&self) -> Selection {
        self.selection.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The hardware scan is in: automatic selection can use it.
    pub fn set_hardware(&self, gpus: Vec<Gpu>) {
        *self.hardware.lock().unwrap_or_else(|e| e.into_inner()) = Some(gpus);
        self.hardware_ready.notify_all();
    }

    fn find(&self, variant: &str) -> Option<&PinnedVariant> {
        self.variants.iter().find(|v| v.variant == variant)
    }

    fn install_dir(&self, variant: &str) -> PathBuf {
        self.root.join(format!("{variant}-{}", self.version))
    }

    fn marker_text(variant: &PinnedVariant) -> String {
        variant
            .archives
            .iter()
            .map(|a| a.sha256.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `llama-server` of `variant`, if it's installed.
    pub fn server_exe(&self, variant: &str) -> Option<PathBuf> {
        let v = self.find(variant)?;
        let dir = self.install_dir(variant);
        let marker = std::fs::read_to_string(dir.join(MARKER)).ok()?;
        let exe = dir.join(SERVER_EXE);
        (marker.trim() == Self::marker_text(v) && exe.is_file()).then_some(exe)
    }

    /// The candidates in order, without the failed ones. `gpus` None: the
    /// hardware isn't known yet, so only an explicit choice and `cpu`.
    fn candidates_with(&self, gpus: Option<&[Gpu]>) -> Vec<String> {
        let mut list = match self.selection() {
            Selection::Explicit(v) => vec![v, "cpu".into()],
            Selection::Auto => match gpus {
                Some(gpus) => select::automatic(gpus, &self.variants),
                None => vec!["cpu".into()],
            },
        };
        list.dedup();
        let state = self.state();
        list.retain(|v| self.find(v).is_some() && !state.failed.contains_key(v));
        list
    }

    /// Candidates once the hardware is known (or the wait is over).
    fn candidates_waiting(&self) -> Vec<String> {
        let guard = self.hardware.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = self
            .hardware_ready
            .wait_timeout_while(guard, self.hardware_wait, |h| h.is_none())
            .unwrap_or_else(|e| e.into_inner());
        let gpus = guard.clone();
        drop(guard);
        if gpus.is_none() && self.selection() == Selection::Auto {
            warn!(target: "runtime", "the hardware scan isn't done; choosing a runtime without it");
        }
        self.candidates_with(gpus.as_deref())
    }

    /// The variant models load with: what the last load used, or else the
    /// best candidate.
    pub fn selected(&self) -> Option<String> {
        if let Some(active) = self.state().active.clone() {
            return Some(active);
        }
        let gpus = self.hardware.lock().unwrap_or_else(|e| e.into_inner()).clone();
        self.candidates_with(gpus.as_deref()).into_iter().next()
    }

    /// Every pinned variant, for the inventory.
    pub fn info(&self) -> Vec<RuntimeInfo> {
        let selected = self.selected();
        let automatic = self.selection() == Selection::Auto;
        let state = self.state();
        self.variants
            .iter()
            .map(|v| {
                let name = &v.variant;
                let runtime_state = if state.downloading.as_deref() == Some(name) {
                    RuntimeState::Downloading
                } else if state.failed.contains_key(name) {
                    RuntimeState::Failed
                } else if self.server_exe(name).is_some() {
                    RuntimeState::Ready
                } else {
                    RuntimeState::Absent
                };
                RuntimeInfo {
                    backend: Backend::LlamaCpp,
                    version: self.version.clone(),
                    variant: name.clone(),
                    state: runtime_state,
                    selected: selected.as_deref() == Some(name),
                    automatic,
                    devices: state.devices.get(name).cloned().unwrap_or_default(),
                    error: state.failed.get(name).cloned(),
                }
            })
            .collect()
    }

    /// Records that `variant` doesn't work here; it's skipped from now on.
    pub fn mark_failed(&self, variant: &str, why: &str) {
        warn!(target: "runtime", "llama.cpp {variant} doesn't work here: {why}");
        let mut state = self.state();
        state.failed.insert(variant.to_string(), why.to_string());
        state.devices.remove(variant);
        if state.active.as_deref() == Some(variant) {
            state.active = None;
        }
    }

    /// The server's choice. "auto" restores the automatic one. Persisted;
    /// clears a failure of the variant chosen, so it's tried again.
    pub fn select(&self, variant: &str) -> Result<(), String> {
        let selection = if variant == "auto" {
            Selection::Auto
        } else if self.find(variant).is_some() {
            Selection::Explicit(variant.to_string())
        } else {
            let pinned: Vec<&str> = self.variants.iter().map(|v| v.variant.as_str()).collect();
            return Err(format!(
                "{variant:?} isn't a llama.cpp variant this client has pinned (it has: auto, {})",
                pinned.join(", ")
            ));
        };
        if let Selection::Explicit(v) = &selection {
            self.state().failed.remove(v);
        }
        let _ = std::fs::create_dir_all(&self.root);
        let json = serde_json::to_vec(&selection).map_err(|e| e.to_string())?;
        store::write_atomically(&self.root.join(SELECTION_FILE), &json)
            .map_err(|e| format!("couldn't save the choice: {e}"))?;
        *self.selection.lock().unwrap_or_else(|e| e.into_inner()) = selection;
        // The next load chooses again.
        self.state().active = None;
        Ok(())
    }

    /// The variant to run, installing and checking candidates in order
    /// until one works. Blocking. `progress` is (done, total) bytes of
    /// downloads; `changed` is called when the inventory's view changes.
    pub fn resolve(
        &self,
        agent: &ureq::Agent,
        cancel: &AtomicBool,
        reserve_bytes: u64,
        progress: &mut dyn FnMut(u64, u64),
        changed: &dyn Fn(),
    ) -> Result<Resolved, PullError> {
        let candidates = self.candidates_waiting();
        let _one_at_a_time = self.install.lock().unwrap_or_else(|e| e.into_inner());
        let mut last_error = None;
        for variant in candidates {
            // Another caller may have failed it while we waited.
            if self.state().failed.contains_key(&variant) {
                continue;
            }
            match self.ready(&variant, agent, cancel, reserve_bytes, progress, changed) {
                Ok(exe) => {
                    let switched = {
                        let mut state = self.state();
                        let switched = state.active.as_deref() != Some(variant.as_str());
                        state.active = Some(variant.clone());
                        switched
                    };
                    if switched {
                        info!(target: "runtime", "using llama.cpp {} ({variant})", self.version);
                        self.remove_unused(&variant);
                        changed();
                    }
                    return Ok(Resolved { variant, exe });
                }
                Err(PullError::Cancelled) => return Err(PullError::Cancelled),
                Err(PullError::Failed(why)) => {
                    self.mark_failed(&variant, &why);
                    changed();
                    last_error = Some(format!("{variant}: {why}"));
                }
            }
        }
        Err(PullError::Failed(match last_error {
            Some(why) => format!("no llama.cpp build works here; the last one tried failed ({why})"),
            None => format!(
                "llama.cpp isn't available for this platform ({} {})",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        }))
    }

    /// `variant` installed and, for a GPU variant, checked.
    fn ready(
        &self,
        variant: &str,
        agent: &ureq::Agent,
        cancel: &AtomicBool,
        reserve_bytes: u64,
        progress: &mut dyn FnMut(u64, u64),
        changed: &dyn Fn(),
    ) -> Result<PathBuf, PullError> {
        let v = self
            .find(variant)
            .ok_or_else(|| PullError::Failed("not pinned".into()))?
            .clone();
        let exe = match self.server_exe(variant) {
            Some(exe) => exe,
            None => {
                self.state().downloading = Some(variant.to_string());
                changed();
                let result = self.install(&v, agent, cancel, reserve_bytes, progress);
                self.state().downloading = None;
                changed();
                result?
            }
        };
        if variant != "cpu" && !self.state().devices.contains_key(variant) {
            let devices = probe(&exe, variant).map_err(PullError::Failed)?;
            info!(target: "runtime", "llama.cpp {variant} sees: {}", devices.join(", "));
            self.state().devices.insert(variant.to_string(), devices);
            changed();
        }
        Ok(exe)
    }

    /// After a restart: checks the installed variant that would be used,
    /// so the inventory shows its devices (or that it fails) before any
    /// load. Blocking; cheap if there's nothing to check.
    pub fn check_installed(&self, changed: &dyn Fn()) {
        let Some(variant) = self.selected() else { return };
        if variant == "cpu" || self.state().devices.contains_key(&variant) {
            return;
        }
        let _one_at_a_time = self.install.lock().unwrap_or_else(|e| e.into_inner());
        let Some(exe) = self.server_exe(&variant) else { return };
        match probe(&exe, &variant) {
            Ok(devices) => {
                info!(target: "runtime", "llama.cpp {variant} sees: {}", devices.join(", "));
                self.state().devices.insert(variant, devices);
            }
            Err(why) => self.mark_failed(&variant, &why),
        }
        changed();
    }

    fn install(
        &self,
        v: &PinnedVariant,
        agent: &ureq::Agent,
        cancel: &AtomicBool,
        reserve_bytes: u64,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<PathBuf, PullError> {
        let io = |what: &str, e: std::io::Error| PullError::Failed(format!("{what}: {e}"));
        std::fs::create_dir_all(&self.root).map_err(|e| io("couldn't create the runtime directory", e))?;
        let name = format!("{}-{}", v.variant, self.version);
        let partial = |i: usize| self.root.join(format!("{name}.{i}.partial"));

        // Up front: every download, plus room to unpack them.
        let total = v.total_bytes();
        let have: u64 = v
            .archives
            .iter()
            .enumerate()
            .map(|(i, a)| std::fs::metadata(partial(i)).map_or(0, |m| m.len().min(a.size_bytes)))
            .sum();
        let needed = (total - have) + total * UNPACK_FACTOR;
        if let Some(free) = (self.free_space)(&self.root) {
            if free.saturating_sub(reserve_bytes) < needed {
                return Err(PullError::Failed(format!(
                    "not enough disk space for llama.cpp {}: it needs about {}, and {} is free with {} kept in \
                     reserve (INFERENCE_DISK_RESERVE_MB)",
                    v.variant,
                    store::mib(needed),
                    store::mib(free),
                    store::mib(reserve_bytes)
                )));
            }
        }

        // Every archive downloaded and verified before anything is unpacked.
        let mut before = 0;
        for (i, a) in v.archives.iter().enumerate() {
            info!(target: "runtime", "downloading llama.cpp {} {} ({})", self.version, v.variant, a.url);
            store::download(
                agent,
                &Download {
                    url: &a.url,
                    sha256: &a.sha256,
                    size: a.size_bytes,
                    partial: &partial(i),
                },
                cancel,
                &mut |done, _| progress(before + done, total),
            )?;
            before += a.size_bytes;
        }

        // Verified; unpack beside the final directory, then swap it in.
        let dir = self.install_dir(&v.variant);
        let staging = self.root.join(format!("{name}.unpacking"));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| io("couldn't create the runtime directory", e))?;
        let mut unpacked = 0;
        for (i, a) in v.archives.iter().enumerate() {
            let result = match a.format {
                ArchiveFormat::TarGz => unpack_tar_gz(&partial(i), &staging),
                ArchiveFormat::Zip => unpack_zip(&partial(i), &staging),
            };
            match result {
                Ok(n) => unpacked += n,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&staging);
                    return Err(PullError::Failed(format!("couldn't unpack llama.cpp: {e}")));
                }
            }
        }
        let exe = staging.join(SERVER_EXE);
        if !exe.is_file() {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(PullError::Failed(format!("the llama.cpp archive has no {SERVER_EXE}")));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| io("couldn't make llama-server executable", e))?;
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::rename(&staging, &dir).map_err(|e| io("couldn't move the runtime into place", e))?;
        store::write_atomically(&dir.join(MARKER), Self::marker_text(v).as_bytes())
            .map_err(|e| io("couldn't finish installing the runtime", e))?;
        for i in 0..v.archives.len() {
            if let Err(e) = std::fs::remove_file(partial(i)) {
                warn!(target: "runtime", "couldn't remove {}: {e}", partial(i).display());
            }
        }
        info!(target: "runtime", "llama.cpp {} {} ready ({unpacked} files)", self.version, v.variant);
        Ok(dir.join(SERVER_EXE))
    }

    /// After switching to `keep`: other variants of this version go, except
    /// `cpu` (the fallback). A directory still in use (Windows) stays until
    /// next time.
    fn remove_unused(&self, keep: &str) {
        for v in &self.variants {
            if v.variant == keep || v.variant == "cpu" {
                continue;
            }
            let dir = self.install_dir(&v.variant);
            if dir.exists() {
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => info!(target: "runtime", "removed llama.cpp {} (no longer used)", v.variant),
                    Err(e) => warn!(target: "runtime", "couldn't remove {}: {e}", dir.display()),
                }
            }
        }
    }

    /// Runtimes (and downloads) of other llama.cpp versions.
    fn remove_other_versions(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(version) = KNOWN_VARIANTS
                .iter()
                .find_map(|v| name.strip_prefix(&format!("{v}-")))
                .map(|rest| rest.split('.').next().unwrap_or(rest))
            else {
                continue;
            };
            if version == self.version || LlamaCppVersion::parse(version).is_none() {
                continue;
            }
            let path = entry.path();
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match removed {
                Ok(()) => info!(target: "runtime", "removed {name} (an older llama.cpp)"),
                Err(e) => warn!(target: "runtime", "couldn't remove {}: {e}", path.display()),
            }
        }
    }
}

fn read_selection(root: &Path, variants: &[PinnedVariant]) -> Selection {
    let Ok(text) = std::fs::read_to_string(root.join(SELECTION_FILE)) else {
        return Selection::Auto;
    };
    match serde_json::from_str::<Selection>(&text) {
        Ok(Selection::Explicit(v)) if variants.iter().any(|p| p.variant == v) => Selection::Explicit(v),
        Ok(Selection::Auto) => Selection::Auto,
        _ => {
            warn!(target: "runtime", "ignoring the saved runtime choice ({}): not a pinned variant", text.trim());
            Selection::Auto
        }
    }
}

// ---------------- does a variant see a GPU? ----------------

/// The ggml device name prefix of a variant's backend.
fn device_prefix(variant: &str) -> &'static str {
    if variant.starts_with("cuda") {
        "CUDA"
    } else if variant == "vulkan" {
        "Vulkan"
    } else {
        "\0"
    }
}

/// The GPUs of `variant`'s backend in `llama-server --list-devices`
/// output. In b12110 that's "Available devices:" and then one line per
/// device, e.g. "  CUDA0: NVIDIA GeForce RTX 3070 (8191 MiB, 7300 MiB free)",
/// or "  (none)".
pub fn parse_devices(output: &str, variant: &str) -> Vec<String> {
    let prefix = device_prefix(variant);
    output
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.trim().split_once(": ")?;
            let index = name.strip_prefix(prefix)?;
            if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let description = match rest.rfind(" (") {
                Some(i) if rest.ends_with(" MiB free)") => &rest[..i],
                _ => rest,
            };
            Some(description.trim().to_string()).filter(|d| !d.is_empty())
        })
        .collect()
}

/// Runs `exe --list-devices`: the GPUs it can use, or why it can't.
pub fn probe(exe: &Path, variant: &str) -> Result<Vec<String>, String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--list-devices")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    process::before_spawn(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("couldn't start it: {e}"))?;
    process::after_spawn_std(&child);
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(pipe) = pipe {
                let _ = pipe.take(256 * 1024).read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = read(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = read(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= PROBE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("couldn't wait for it: {e}")),
        }
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    let tail = || {
        let all = format!("{err}\n{out}");
        let lines: Vec<&str> = all.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = lines[lines.len().saturating_sub(12)..].join("\n");
        if tail.is_empty() {
            "(it printed nothing)".to_string()
        } else {
            tail
        }
    };
    let Some(status) = status else {
        return Err(format!(
            "--list-devices didn't finish within {} s. Its output:\n{}",
            PROBE_TIMEOUT.as_secs(),
            tail()
        ));
    };
    if !status.success() {
        return Err(format!("--list-devices failed ({status}). Its output:\n{}", tail()));
    }
    let devices = parse_devices(&out, variant);
    if devices.is_empty() {
        return Err(format!(
            "it found no {} GPU (a missing driver{}, or no GPU access in a container?). Its output:\n{}",
            device_prefix(variant),
            if variant == "vulkan" { " or Vulkan loader" } else { "" },
            tail()
        ));
    }
    Ok(devices)
}

// ---------------- unpacking ----------------

/// What `llama-server` needs from a release archive: itself, the shared
/// libraries it loads (ggml's CPU backends, the GPU backends `ggml-cuda`
/// and `ggml-vulkan`, and the CUDA runtime and cuBLAS from the `cudart`
/// archives), and the licences. The other tools and their `-impl`
/// libraries are left out.
pub fn wanted(name: &str) -> bool {
    let library = name.ends_with(".dll") || name.ends_with(".so") || name.contains(".so.");
    let other_tool = name.contains("-impl") && !name.contains("llama-server-impl");
    let server = name == "llama-server" || name == "llama-server.exe";
    server || name.starts_with("LICENSE") || library && !other_tool
}

/// A bare file name: never a path, never `.` or `..`.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0', ':'])
}

/// The archive's files flattened into `dest`. Entries are at most one
/// directory deep in the releases (`llama-b12110/llama-server`,
/// `cudart-llama-…/libcudart.so.13`); anything deeper is skipped, so
/// nothing can be written outside `dest`.
fn entry_name(path: &Path) -> Option<String> {
    use std::path::Component;
    let parts: Vec<String> = path
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Option<_>>()?;
    match parts.as_slice() {
        [name] | [_, name] if plain_name(name) => Some(name.clone()),
        _ => None,
    }
}

fn unpack_tar_gz(archive: &Path, dest: &Path) -> std::io::Result<usize> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?));
    let mut count = 0;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let Some(name) = entry_name(&entry.path()?) else {
            continue;
        };
        if !wanted(&name) {
            continue;
        }
        let target = dest.join(&name);
        match entry.header().entry_type() {
            tar::EntryType::Regular => {
                entry.unpack(&target)?;
                count += 1;
            }
            // libllama.so -> libllama.so.0: only links to a file beside it.
            tar::EntryType::Symlink => {
                let link = entry.link_name()?.map(|l| l.to_string_lossy().into_owned());
                match link {
                    Some(link) if plain_name(&link) => {
                        #[cfg(unix)]
                        std::os::unix::fs::symlink(&link, &target)?;
                        #[cfg(not(unix))]
                        let _ = link;
                        count += 1;
                    }
                    _ => warn!(target: "runtime", "skipping the link {name} in the archive"),
                }
            }
            _ => {}
        }
    }
    Ok(count)
}

fn unpack_zip(archive: &Path, dest: &Path) -> std::io::Result<usize> {
    let mut zip = zip::ZipArchive::new(File::open(archive)?).map_err(std::io::Error::other)?;
    let mut count = 0;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(std::io::Error::other)?;
        if !file.is_file() {
            continue;
        }
        let Some(name) = file.enclosed_name().as_deref().and_then(entry_name) else {
            continue;
        };
        if !wanted(&name) {
            continue;
        }
        let mut out = File::create(dest.join(&name))?;
        std::io::copy(&mut file, &mut out)?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::inference::store::tests::{serve, sha, Quirks, TempDir};
    use std::io::Write;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn the_embedded_pins_are_valid() {
        // Not checking the version: the weekly update workflow moves it.
        let pins = parse_pins(EMBEDDED_PINS).unwrap();
        for (os, arch) in [("linux", "x86_64"), ("linux", "aarch64"), ("windows", "x86_64")] {
            let cpu: Vec<_> = pins
                .variants
                .iter()
                .filter(|v| v.os == os && v.arch == arch && v.variant == "cpu")
                .collect();
            assert_eq!(cpu.len(), 1, "{os} {arch}");
            for v in pins.variants.iter().filter(|v| v.os == os && v.arch == arch) {
                for a in &v.archives {
                    assert_eq!(a.format == ArchiveFormat::Zip, os == "windows", "{}", a.url);
                }
            }
        }
        // CUDA builds come with their CUDA runtime.
        for v in pins.variants.iter().filter(|v| v.variant.starts_with("cuda")) {
            assert_eq!(v.archives.len(), 2, "{} {}", v.os, v.variant);
            assert!(v.archives[1].url.contains("/cudart-llama-"), "{}", v.archives[1].url);
        }
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            let here: Vec<String> = pinned_for_this_platform().into_iter().map(|v| v.variant).collect();
            assert_eq!(here, ["cpu", "cuda-12", "cuda-13", "vulkan"]);
        }
    }

    #[test]
    fn bad_pins_are_caught() {
        let good: serde_json::Value = serde_json::from_str(EMBEDDED_PINS).unwrap();
        let check = |edit: &dyn Fn(&mut serde_json::Value), expect: &str| {
            let mut v = good.clone();
            edit(&mut v);
            let r = parse_pins(&v.to_string());
            assert!(matches!(&r, Err(why) if why.contains(expect)), "{expect}: {r:?}");
        };
        check(&|v| v["schema"] = 2.into(), "unknown schema");
        check(&|v| v["version"] = "12110".into(), "isn't a llama.cpp version");
        check(
            &|v| {
                v["variants"][1]["archives"][0]["url"] =
                    "https://evil.example.com/llama-b12110-bin-ubuntu-x64.tar.gz".into()
            },
            "isn't a file of the b12110 release",
        );
        check(
            &|v| {
                v["variants"][1]["archives"][0]["url"] =
                    "http://github.com/ggml-org/llama.cpp/releases/download/b12110/x.tar.gz".into()
            },
            "isn't a file of the b12110 release",
        );
        check(
            &|v| {
                v["variants"][1]["archives"][0]["url"] =
                    "https://github.com/ggml-org/llama.cpp/releases/download/b9999/x.tar.gz".into()
            },
            "isn't a file of the b12110 release",
        );
        check(&|v| v["variants"][0]["archives"][0]["sha256"] = "ABC".into(), "sha256");
        check(
            &|v| v["variants"][0]["archives"][0]["sha256"] = "A".repeat(64).into(),
            "sha256",
        );
        check(&|v| v["variants"][0]["archives"][0]["size_bytes"] = 0.into(), "size");
        check(&|v| v["variants"][1]["variant"] = "rocm".into(), "unknown variant");
        check(
            &|v| {
                let dup = v["variants"][1].clone();
                v["variants"].as_array_mut().unwrap().push(dup);
            },
            "pinned twice",
        );
        check(&|v| v["variants"][1]["requires"] = serde_json::json!({}), "cuda_driver");
        check(
            &|v| v["variants"][0]["archives"][0]["format"] = "zip".into(),
            "isn't a Zip archive",
        );
        check(&|v| v["variants"][0]["surprise"] = 1.into(), "unknown field");
        check(
            &|v| {
                let arr = v["variants"].as_array_mut().unwrap();
                arr.retain(|x| !(x["os"] == "windows" && x["variant"] == "cpu"));
            },
            "cpu variant",
        );
    }

    #[test]
    fn only_what_llama_server_needs_is_unpacked() {
        // Names from the b12110 archives' listings.
        for keep in [
            "llama-server",
            "libllama-server-impl.so",
            "libllama.so.0.9.0",
            "libllama-common.so.0.9.0",
            "libggml-cpu-haswell.so",
            "libggml-cuda.so",
            "libggml-vulkan.so",
            "libcudart.so.13",
            "libcublas.so.12",
            "libcublasLt.so.13",
            "LICENSE",
            "LICENSE-LLVM-OpenMP",
            "ggml-base.dll",
            "ggml-cuda.dll",
            "ggml-vulkan.dll",
            "cudart64_12.dll",
            "cublas64_13.dll",
            "cublasLt64_12.dll",
            "libomp.dll",
            "llama-server-impl.dll",
        ] {
            assert!(wanted(keep), "{keep}");
        }
        for skip in [
            "llama-cli",
            "llama-bench",
            "libllama-cli-impl.so",
            "llama-cli-impl.dll",
            "llama-tts.exe",
        ] {
            assert!(!wanted(skip), "{skip}");
        }
        assert_eq!(
            entry_name(Path::new("llama-b1/llama-server")).as_deref(),
            Some("llama-server")
        );
        assert_eq!(
            entry_name(Path::new("cudart-llama-b1-bin-ubuntu-cuda-13.4-x64/libcudart.so.13")).as_deref(),
            Some("libcudart.so.13")
        );
        assert_eq!(entry_name(Path::new("ggml.dll")).as_deref(), Some("ggml.dll"));
        assert_eq!(entry_name(Path::new("a/b/c.so")), None);
        assert_eq!(entry_name(Path::new("../evil.so")), None);
        assert_eq!(entry_name(Path::new("/abs/evil.so")), None);
    }

    #[test]
    fn devices_are_read_from_list_devices() {
        let cuda = "ggml_cuda_init: found 1 CUDA devices:\n  Device 0: NVIDIA GeForce RTX 3070, compute capability 8.6, VMM: yes\n\
                    Available devices:\n  CUDA0: NVIDIA GeForce RTX 3070 (8191 MiB, 7311 MiB free)\n";
        assert_eq!(parse_devices(cuda, "cuda-13"), ["NVIDIA GeForce RTX 3070"]);
        assert!(
            parse_devices(cuda, "vulkan").is_empty(),
            "another backend's devices don't count"
        );
        let vulkan =
            "Available devices:\n  Vulkan0: AMD Radeon RX 7900 XTX (RADV NAVI31) (24560 MiB, 24000 MiB free)\n  \
                      Vulkan1: Intel(R) Iris(R) Xe Graphics (RPL-P) (7824 MiB, 7824 MiB free)\n";
        assert_eq!(
            parse_devices(vulkan, "vulkan"),
            [
                "AMD Radeon RX 7900 XTX (RADV NAVI31)",
                "Intel(R) Iris(R) Xe Graphics (RPL-P)"
            ]
        );
        assert!(parse_devices("Available devices:\n  (none)\n", "vulkan").is_empty());
        assert!(parse_devices("Available devices:\n  BLAS: OpenBLAS (0 MiB, 0 MiB free)\n", "cuda-12").is_empty());
    }

    /// A tarball shaped like a release: a top directory, the server, a
    /// library with a symlink, a tool we don't want, and two hostile entries.
    /// `server` is the llama-server file's contents.
    pub fn fake_release(server: &[u8], top: &str, extra_lib: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        let mut add = |path: &str, body: &[u8], kind: tar::EntryType, link: Option<&str>| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            if let Some(link) = link {
                header.set_link_name(link).unwrap();
            }
            // set_path refuses "..", so write the name bytes directly.
            let name = &mut header.as_old_mut().name;
            name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_cksum();
            builder.append(&header, body).unwrap();
        };
        if !server.is_empty() {
            add(&format!("{top}/llama-server"), server, tar::EntryType::Regular, None);
            add(
                &format!("{top}/libllama.so.0.9.0"),
                b"lib",
                tar::EntryType::Regular,
                None,
            );
            add(
                &format!("{top}/libllama.so.0"),
                b"",
                tar::EntryType::Symlink,
                Some("libllama.so.0.9.0"),
            );
            add(
                &format!("{top}/libevil.so"),
                b"",
                tar::EntryType::Symlink,
                Some("/etc/passwd"),
            );
            add(&format!("{top}/llama-cli"), b"cli", tar::EntryType::Regular, None);
            add(&format!("{top}/../../escape.so"), b"bad", tar::EntryType::Regular, None);
        }
        if !extra_lib.is_empty() {
            add(
                &format!("{top}/{extra_lib}"),
                b"runtime lib",
                tar::EntryType::Regular,
                None,
            );
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    pub fn archive_for(body: &[u8], url: String) -> PinnedArchive {
        PinnedArchive {
            url,
            sha256: sha(body),
            size_bytes: body.len() as u64,
            format: ArchiveFormat::TarGz,
        }
    }

    pub fn variant(name: &str, archives: Vec<PinnedArchive>) -> PinnedVariant {
        PinnedVariant {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            variant: name.into(),
            requires: Requires::default(),
            archives,
        }
    }

    pub fn runtime_with(dir: &Path, variants: Vec<PinnedVariant>) -> Runtime {
        Runtime::with_parts(
            dir,
            RuntimeParts {
                version: "b12110".into(),
                variants,
                free_space: Arc::new(|_| None),
                hardware_wait: Duration::from_millis(10),
            },
        )
    }

    fn resolve(runtime: &Runtime) -> Result<Resolved, PullError> {
        runtime.resolve(
            &store::http_agent(true),
            &AtomicBool::new(false),
            0,
            &mut |_, _| {},
            &|| {},
        )
    }

    #[cfg(unix)]
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    // The Linux releases are tarballs with symlinks; Windows gets a zip.
    #[cfg(unix)]
    #[test]
    fn the_runtime_is_downloaded_verified_and_unpacked_once() {
        let dir = TempDir::new("runtime");
        let body = fake_release(b"#!server", "llama-b1", "");
        let (url, requests) = serve(body.clone(), Quirks::default());
        let runtime = runtime_with(&dir.0, vec![variant("cpu", vec![archive_for(&body, url)])]);
        assert_eq!(runtime.info()[0].state, RuntimeState::Absent);
        let changes = AtomicUsize::new(0);
        let mut reports = Vec::new();
        let resolved = runtime
            .resolve(
                &store::http_agent(true),
                &AtomicBool::new(false),
                0,
                &mut |done, total| reports.push((done, total)),
                &|| {
                    changes.fetch_add(1, Ordering::Relaxed);
                },
            )
            .unwrap();
        assert_eq!(resolved.variant, "cpu");
        assert!(changes.load(Ordering::Relaxed) >= 2, "downloading, then done");
        assert_eq!(reports.last(), Some(&(body.len() as u64, body.len() as u64)));
        let exe = resolved.exe;
        assert_eq!(std::fs::read(&exe).unwrap(), b"#!server");
        let installed = exe.parent().unwrap();
        assert_eq!(
            names_in(installed),
            vec![".installed", "libllama.so.0", "libllama.so.0.9.0", "llama-server"]
        );
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o111, 0o111);
            assert_eq!(std::fs::read(installed.join("libllama.so.0")).unwrap(), b"lib");
        }
        assert!(!dir.0.join("escape.so").exists() && !dir.0.join("runtime/escape.so").exists());
        let info = &runtime.info()[0];
        assert_eq!(
            (info.state, info.selected, info.automatic),
            (RuntimeState::Ready, true, true)
        );
        assert_eq!(info.version, "b12110");

        // Already there: no second download.
        resolve(&runtime).unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_variant_in_two_archives_is_verified_whole_before_unpacking() {
        let dir = TempDir::new("runtime-two");
        let build = fake_release(b"#!server", "llama-b1", "");
        let cudart = fake_release(b"", "cudart-llama-b1", "libcudart.so.13");
        let (build_url, _) = serve(build.clone(), Quirks::default());
        let (cudart_url, _) = serve(cudart.clone(), Quirks::default());

        // A bad hash on the second archive: nothing is unpacked at all.
        let mut bad = archive_for(&cudart, cudart_url.clone());
        bad.sha256 = sha(b"the real cudart");
        let runtime = runtime_with(
            &dir.0,
            vec![variant("cpu", vec![archive_for(&build, build_url.clone()), bad])],
        );
        let r = resolve(&runtime);
        assert!(
            matches!(&r, Err(PullError::Failed(why)) if why.contains("hash mismatch")),
            "{r:?}"
        );
        assert!(!dir.0.join("runtime/cpu-b12110").exists());
        assert!(!dir.0.join("runtime/cpu-b12110.unpacking").exists());
        assert!(
            !dir.0.join("runtime/cpu-b12110.1.partial").exists(),
            "the bad one is deleted"
        );
        drop(runtime);

        // Both good: unpacked into one directory, progress over both.
        let runtime = runtime_with(
            &dir.0,
            vec![variant(
                "cpu",
                vec![archive_for(&build, build_url), archive_for(&cudart, cudart_url)],
            )],
        );
        let total = (build.len() + cudart.len()) as u64;
        let mut last = (0, 0);
        let resolved = runtime
            .resolve(
                &store::http_agent(true),
                &AtomicBool::new(false),
                0,
                &mut |done, t| last = (done, t),
                &|| {},
            )
            .unwrap();
        assert_eq!(last, (total, total));
        let names = names_in(resolved.exe.parent().unwrap());
        assert!(names.contains(&"libcudart.so.13".to_string()), "{names:?}");
        assert!(names.contains(&"llama-server".to_string()));
        assert!(!dir.0.join("runtime/cpu-b12110.0.partial").exists());
    }

    #[test]
    fn the_disk_check_counts_every_archive() {
        let dir = TempDir::new("runtime-disk");
        let a = archive_for(b"x", "http://127.0.0.1:9/a.tar.gz".into());
        let mut b = a.clone();
        b.size_bytes = 400 << 20;
        let runtime = Runtime::with_parts(
            &dir.0,
            RuntimeParts {
                version: "b12110".into(),
                variants: vec![variant("cpu", vec![a, b])],
                free_space: Arc::new(|_| Some(1 << 30)),
                hardware_wait: Duration::ZERO,
            },
        );
        let r = resolve(&runtime);
        assert!(
            matches!(&r, Err(PullError::Failed(why)) if why.contains("not enough disk space")),
            "{r:?}"
        );
    }

    /// A release whose llama-server is a shell script printing `devices`
    /// for `--list-devices` (or failing with `exit`).
    #[cfg(unix)]
    fn scripted(name: &str, script: &str) -> (PinnedVariant, Arc<AtomicUsize>) {
        let body = fake_release(format!("#!/bin/sh\n{script}\n").as_bytes(), "llama-b1", "");
        let (url, requests) = serve(body.clone(), Quirks::default());
        (variant(name, vec![archive_for(&body, url)]), requests)
    }

    #[cfg(unix)]
    #[test]
    fn a_gpu_variant_that_sees_no_gpu_fails_and_the_next_one_is_used() {
        let dir = TempDir::new("runtime-fallback");
        let (cpu, _) = scripted("cpu", "echo 'Available devices:'; echo '  (none)'");
        let (vulkan, _) = scripted("vulkan", "echo 'Available devices:'; echo '  (none)'");
        let (cuda, _) = scripted("cuda-13", "echo 'ggml_cuda_init: no CUDA devices' >&2; exit 3");
        let runtime = runtime_with(&dir.0, vec![cpu, cuda, vulkan]);
        runtime.set_hardware(vec![Gpu {
            vendor: Some("NVIDIA".into()),
            cuda_driver_version: Some("13.4".into()),
            compute_capability: Some("8.6".into()),
            ..Gpu::default()
        }]);
        // The test pins have no cuda requirement, so make cuda-13 the explicit choice.
        runtime.select("cuda-13").unwrap();
        let resolved = resolve(&runtime).unwrap();
        assert_eq!(resolved.variant, "cpu");
        let info = runtime.info();
        let cuda = info.iter().find(|r| r.variant == "cuda-13").unwrap();
        assert_eq!(cuda.state, RuntimeState::Failed);
        let why = cuda.error.as_deref().unwrap();
        assert!(why.contains("failed") && why.contains("no CUDA devices"), "{why}");
        assert!(info.iter().find(|r| r.variant == "cpu").unwrap().selected);
        assert!(!info[0].automatic);

        // Back to automatic on a machine with an AMD card: vulkan is tried,
        // sees nothing, and fails too; cpu stays.
        runtime.select("auto").unwrap();
        runtime.set_hardware(vec![Gpu {
            vendor: Some("AMD".into()),
            ..Gpu::default()
        }]);
        assert_eq!(resolve(&runtime).unwrap().variant, "cpu");
        let vulkan = runtime.info().into_iter().find(|r| r.variant == "vulkan").unwrap();
        assert_eq!(vulkan.state, RuntimeState::Failed);
        assert!(vulkan.error.unwrap().contains("found no Vulkan GPU"));
        // Not tried again until it's selected explicitly.
        assert_eq!(resolve(&runtime).unwrap().variant, "cpu");
    }

    #[cfg(unix)]
    #[test]
    fn a_working_gpu_variant_is_used_and_the_unused_ones_removed() {
        let dir = TempDir::new("runtime-gpu");
        let (cpu, _) = scripted("cpu", "exit 0");
        let (vulkan, _) = scripted(
            "vulkan",
            "echo 'Available devices:'; echo '  Vulkan0: AMD Radeon RX 6600 (RADV NAVI23) (8176 MiB, 8000 MiB free)'",
        );
        let (cuda, _) = scripted(
            "cuda-12",
            "echo 'Available devices:'; echo '  CUDA0: NVIDIA X (1 MiB, 1 MiB free)'",
        );
        let runtime = runtime_with(&dir.0, vec![cpu, cuda, vulkan]);
        runtime.select("cuda-12").unwrap();
        let first = resolve(&runtime).unwrap();
        assert_eq!(first.variant, "cuda-12");
        assert_eq!(
            runtime.info().iter().find(|r| r.variant == "cuda-12").unwrap().devices,
            ["NVIDIA X"]
        );

        runtime.select("vulkan").unwrap();
        let second = resolve(&runtime).unwrap();
        assert_eq!(second.variant, "vulkan");
        let vulkan = runtime.info().into_iter().find(|r| r.variant == "vulkan").unwrap();
        assert!(vulkan.selected && !vulkan.automatic);
        assert_eq!(vulkan.devices, ["AMD Radeon RX 6600 (RADV NAVI23)"]);
        assert!(!dir.0.join("runtime/cuda-12-b12110").exists(), "no longer selected");

        // The choice survives a restart.
        drop(runtime);
        std::fs::create_dir_all(dir.0.join("runtime/cpu-b9000")).unwrap();
        std::fs::write(dir.0.join("runtime/vulkan-b9000.0.partial"), b"old").unwrap();
        let (cpu, _) = scripted("cpu", "exit 0");
        let (vulkan, _) = scripted("vulkan", "exit 1");
        let restarted = runtime_with(&dir.0, vec![cpu, vulkan]);
        assert_eq!(restarted.selection(), Selection::Explicit("vulkan".into()));
        assert!(!dir.0.join("runtime/cpu-b9000").exists(), "older versions are removed");
        assert!(!dir.0.join("runtime/vulkan-b9000.0.partial").exists());
        assert!(dir.0.join("runtime/vulkan-b12110").exists(), "this version's stay");
    }

    #[test]
    fn selecting_an_unpinned_variant_fails_cleanly() {
        let dir = TempDir::new("runtime-select");
        let runtime = runtime_with(&dir.0, vec![variant("cpu", vec![])]);
        let err = runtime.select("rocm").unwrap_err();
        assert!(
            err.contains("isn't a llama.cpp variant this client has pinned"),
            "{err}"
        );
        assert_eq!(runtime.selection(), Selection::Auto);
        runtime.select("cpu").unwrap();
        assert_eq!(runtime.selection(), Selection::Explicit("cpu".into()));
    }

    #[test]
    fn a_tampered_runtime_is_never_unpacked() {
        let dir = TempDir::new("runtime-bad");
        let body = fake_release(b"#!server", "llama-b1", "");
        let (url, _) = serve(body.clone(), Quirks::default());
        let mut archive = archive_for(&body, url);
        archive.sha256 = sha(b"the real release");
        let runtime = runtime_with(&dir.0, vec![variant("cpu", vec![archive])]);
        let r = resolve(&runtime);
        assert!(
            matches!(&r, Err(PullError::Failed(why)) if why.contains("hash mismatch")),
            "{r:?}"
        );
        assert!(runtime.server_exe("cpu").is_none());
        assert!(!dir.0.join("runtime/cpu-b12110").exists());
    }

    #[test]
    fn zips_unpack_flat_and_skip_unwanted_files() {
        let dir = TempDir::new("runtime-zip");
        let path = dir.0.join("r.zip");
        {
            let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
            let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            for (name, body) in [
                (SERVER_EXE, &b"exe"[..]),
                ("ggml.dll", b"dll"),
                ("llama-cli.exe", b"no"),
            ] {
                zip.start_file(name, opts).unwrap();
                zip.write_all(body).unwrap();
            }
            zip.finish().unwrap();
        }
        let out = dir.0.join("out");
        std::fs::create_dir_all(&out).unwrap();
        assert_eq!(unpack_zip(&path, &out).unwrap(), 2);
        assert_eq!(std::fs::read(out.join(SERVER_EXE)).unwrap(), b"exe");
        assert!(!out.join("llama-cli.exe").exists());
    }
}
