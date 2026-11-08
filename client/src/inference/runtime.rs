//! Getting `llama-server`: a pinned llama.cpp release, downloaded the first
//! time a model is pulled or loaded, checked against the SHA-256 recorded
//! here, and unpacked into `runtime/<variant>-<version>/`.
//!
//! **Executables never come from the server.** The server is trusted to
//! choose models (data), but if it could name a binary, a compromised
//! server would be code execution on every volunteer's machine. So the
//! release, its URLs and its hashes are compiled in, and updating llama.cpp
//! means a client release.
//!
//! Only CPU builds are pinned for now. GPU variants (CUDA, Vulkan, ROCm)
//! arrive with the suggestion engine, as more rows in [`PINNED`] and a
//! command choosing among them.

use super::store::{self, Download, PullError};
use log::{info, warn};
use protocol::{Backend, RuntimeInfo, RuntimeState};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// The llama.cpp build every client runs: the one behind release v0.9.0.
pub const LLAMA_CPP_VERSION: &str = "b12110";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archive {
    TarGz,
    Zip,
}

/// One downloadable build.
#[derive(Debug)]
pub struct PinnedAsset {
    pub os: &'static str,
    pub arch: &'static str,
    pub variant: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size_bytes: u64,
    pub archive: Archive,
}

/// Checked against the release's published digests and by downloading
/// each one. The Linux builds are made on Ubuntu; they need glibc 2.34 and
/// GCC 12's libstdc++ (both in Debian bookworm), plus libssl3 and libgomp1.
pub const PINNED: &[PinnedAsset] = &[
    PinnedAsset {
        os: "linux",
        arch: "x86_64",
        variant: "cpu",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b12110/llama-b12110-bin-ubuntu-x64.tar.gz",
        sha256: "368227edcc229be5f9bed16a2223d79b5e41d0ac2e19bcf5825e82291c060e55",
        size_bytes: 23_051_958,
        archive: Archive::TarGz,
    },
    PinnedAsset {
        os: "linux",
        arch: "aarch64",
        variant: "cpu",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b12110/llama-b12110-bin-ubuntu-arm64.tar.gz",
        sha256: "4758b51335e37b4346251f2687d64841ff1c07ecb27fc46323945bb2ec9c44d2",
        size_bytes: 18_723_578,
        archive: Archive::TarGz,
    },
    PinnedAsset {
        os: "windows",
        arch: "x86_64",
        variant: "cpu",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b12110/llama-b12110-bin-win-cpu-x64.zip",
        sha256: "98901e327c252875accaf2d1bf364e10e868843b0b63468baad1aa945cdc5da2",
        size_bytes: 24_708_445,
        archive: Archive::Zip,
    },
];

/// The build for this machine, if there is one.
pub fn pinned_for_this_platform() -> Option<&'static PinnedAsset> {
    PINNED
        .iter()
        .find(|a| a.os == std::env::consts::OS && a.arch == std::env::consts::ARCH && a.variant == "cpu")
}

pub const SERVER_EXE: &str = if cfg!(windows) {
    "llama-server.exe"
} else {
    "llama-server"
};
/// Written last, holding the archive's hash: the install is complete.
const MARKER: &str = ".installed";

pub struct Runtime {
    root: PathBuf,
    asset: Option<&'static PinnedAsset>,
    /// One download at a time; a second caller waits and then finds it done.
    install: Mutex<()>,
    downloading: AtomicBool,
}

impl Runtime {
    pub fn new(data_dir: &Path) -> Runtime {
        Self::with_asset(data_dir, pinned_for_this_platform())
    }

    pub(crate) fn with_asset(data_dir: &Path, asset: Option<&'static PinnedAsset>) -> Runtime {
        let root = data_dir.join("runtime");
        store::remove_stale_partials(&root, 1);
        Runtime {
            root,
            asset,
            install: Mutex::new(()),
            downloading: AtomicBool::new(false),
        }
    }

    pub fn asset(&self) -> Option<&'static PinnedAsset> {
        self.asset
    }

    fn install_dir(&self, asset: &PinnedAsset) -> PathBuf {
        self.root.join(format!("{}-{LLAMA_CPP_VERSION}", asset.variant))
    }

    /// `llama-server`, if the runtime is installed.
    pub fn server_exe(&self) -> Option<PathBuf> {
        let dir = self.install_dir(self.asset?);
        let marker = std::fs::read_to_string(dir.join(MARKER)).ok()?;
        let exe = dir.join(SERVER_EXE);
        (marker.trim() == self.asset?.sha256 && exe.is_file()).then_some(exe)
    }

    pub fn state(&self) -> RuntimeState {
        if self.server_exe().is_some() {
            RuntimeState::Ready
        } else if self.downloading.load(Ordering::Relaxed) {
            RuntimeState::Downloading
        } else {
            RuntimeState::Absent
        }
    }

    /// For the inventory; None where no build is pinned for this platform.
    pub fn info(&self) -> Option<RuntimeInfo> {
        let asset = self.asset?;
        Some(RuntimeInfo {
            backend: Backend::LlamaCpp,
            version: LLAMA_CPP_VERSION.into(),
            variant: asset.variant.into(),
            state: self.state(),
        })
    }

    /// The path to `llama-server`, downloading and unpacking the runtime
    /// first if needed. Blocking. `changed` is called when the state the
    /// inventory reports changes.
    pub fn ensure(
        &self,
        agent: &ureq::Agent,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
        changed: &dyn Fn(),
    ) -> Result<PathBuf, PullError> {
        let Some(asset) = self.asset else {
            return Err(PullError::Failed(format!(
                "llama.cpp isn't available for this platform ({} {})",
                std::env::consts::OS,
                std::env::consts::ARCH
            )));
        };
        if let Some(exe) = self.server_exe() {
            return Ok(exe);
        }
        let _one_at_a_time = self.install.lock().unwrap_or_else(|e| e.into_inner());
        // Someone else may have finished it while we waited.
        if let Some(exe) = self.server_exe() {
            return Ok(exe);
        }
        self.downloading.store(true, Ordering::Relaxed);
        changed();
        let result = self.install(asset, agent, cancel, progress);
        self.downloading.store(false, Ordering::Relaxed);
        changed();
        result
    }

    fn install(
        &self,
        asset: &PinnedAsset,
        agent: &ureq::Agent,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<PathBuf, PullError> {
        let io = |what: &str, e: std::io::Error| PullError::Failed(format!("{what}: {e}"));
        std::fs::create_dir_all(&self.root).map_err(|e| io("couldn't create the runtime directory", e))?;
        let archive = self.root.join(format!("{}-{LLAMA_CPP_VERSION}.partial", asset.variant));
        info!(target: "runtime", "downloading llama.cpp {LLAMA_CPP_VERSION} ({})", asset.url);
        store::download(
            agent,
            &Download {
                url: asset.url,
                sha256: asset.sha256,
                size: asset.size_bytes,
                partial: &archive,
            },
            cancel,
            progress,
        )?;

        // Verified; unpack beside the final directory, then swap it in.
        let dir = self.install_dir(asset);
        let staging = self
            .root
            .join(format!("{}-{LLAMA_CPP_VERSION}.unpacking", asset.variant));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| io("couldn't create the runtime directory", e))?;
        let unpacked = match asset.archive {
            Archive::TarGz => unpack_tar_gz(&archive, &staging),
            Archive::Zip => unpack_zip(&archive, &staging),
        }
        .map_err(|e| PullError::Failed(format!("couldn't unpack llama.cpp: {e}")))?;
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
        store::write_atomically(&dir.join(MARKER), asset.sha256.as_bytes())
            .map_err(|e| io("couldn't finish installing the runtime", e))?;
        if let Err(e) = std::fs::remove_file(&archive) {
            warn!(target: "runtime", "couldn't remove {}: {e}", archive.display());
        }
        info!(target: "runtime", "llama.cpp {LLAMA_CPP_VERSION} ready ({unpacked} files)");
        Ok(dir.join(SERVER_EXE))
    }
}

/// What `llama-server` needs from a release archive: itself, the shared
/// libraries it loads (ggml's CPU backends among them), and the licences.
/// The other tools and their `-impl` libraries are left out.
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
/// directory deep in the releases (`llama-b12110/llama-server`); anything
/// deeper is skipped, so nothing can be written outside `dest`.
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
mod tests {
    use super::*;
    use crate::inference::store::tests::{serve, sha, Quirks, TempDir};
    use std::io::Write;

    #[test]
    fn every_supported_platform_has_one_cpu_build() {
        for (os, arch) in [("linux", "x86_64"), ("linux", "aarch64"), ("windows", "x86_64")] {
            let builds: Vec<_> = PINNED.iter().filter(|a| a.os == os && a.arch == arch).collect();
            assert_eq!(builds.len(), 1, "{os} {arch}");
            let a = builds[0];
            assert!(store::is_sha256_hex(a.sha256));
            assert!(a
                .url
                .starts_with("https://github.com/ggml-org/llama.cpp/releases/download/b12110/"));
            assert_eq!(a.archive == Archive::Zip, os == "windows");
        }
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert!(pinned_for_this_platform().is_some());
        }
    }

    #[test]
    fn only_what_llama_server_needs_is_unpacked() {
        for keep in [
            "llama-server",
            "libllama-server-impl.so",
            "libllama.so.0.9.0",
            "libggml-cpu-haswell.so",
            "LICENSE",
            "ggml-base.dll",
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
        assert_eq!(entry_name(Path::new("ggml.dll")).as_deref(), Some("ggml.dll"));
        assert_eq!(entry_name(Path::new("a/b/c.so")), None);
        assert_eq!(entry_name(Path::new("../evil.so")), None);
        assert_eq!(entry_name(Path::new("/abs/evil.so")), None);
    }

    /// A tarball shaped like a release: a top directory, the server, a
    /// library with a symlink, a tool we don't want, and two hostile entries.
    fn fake_release() -> Vec<u8> {
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
        add("llama-b1/llama-server", b"#!server", tar::EntryType::Regular, None);
        add("llama-b1/libllama.so.0.9.0", b"lib", tar::EntryType::Regular, None);
        add(
            "llama-b1/libllama.so.0",
            b"",
            tar::EntryType::Symlink,
            Some("libllama.so.0.9.0"),
        );
        add("llama-b1/libevil.so", b"", tar::EntryType::Symlink, Some("/etc/passwd"));
        add("llama-b1/llama-cli", b"cli", tar::EntryType::Regular, None);
        add("llama-b1/../../escape.so", b"bad", tar::EntryType::Regular, None);
        builder.into_inner().unwrap().finish().unwrap()
    }

    // The Linux releases are tarballs with symlinks; Windows gets a zip.
    #[cfg(unix)]
    #[test]
    fn the_runtime_is_downloaded_verified_and_unpacked_once() {
        let dir = TempDir::new("runtime");
        let body = fake_release();
        let (url, requests) = serve(body.clone(), Quirks::default());
        let asset: &'static PinnedAsset = Box::leak(Box::new(PinnedAsset {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            variant: "cpu",
            url: Box::leak(url.into_boxed_str()),
            sha256: Box::leak(sha(&body).into_boxed_str()),
            size_bytes: body.len() as u64,
            archive: Archive::TarGz,
        }));
        let runtime = Runtime::with_asset(&dir.0, Some(asset));
        let agent = store::http_agent(true);
        assert_eq!(runtime.state(), RuntimeState::Absent);
        let changes = std::sync::atomic::AtomicUsize::new(0);
        let exe = runtime
            .ensure(&agent, &AtomicBool::new(false), &mut |_, _| {}, &|| {
                changes.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
        assert_eq!(changes.load(Ordering::Relaxed), 2, "downloading, then done");
        assert_eq!(std::fs::read(&exe).unwrap(), b"#!server");
        let installed = exe.parent().unwrap();
        let mut names: Vec<String> = std::fs::read_dir(installed)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![".installed", "libllama.so.0", "libllama.so.0.9.0", "llama-server"]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o111, 0o111);
            assert_eq!(std::fs::read(installed.join("libllama.so.0")).unwrap(), b"lib");
        }
        assert!(!dir.0.join("escape.so").exists() && !dir.0.join("runtime/escape.so").exists());
        assert_eq!(runtime.state(), RuntimeState::Ready);
        assert_eq!(runtime.info().unwrap().version, LLAMA_CPP_VERSION);

        // Already there: no second download.
        runtime
            .ensure(&agent, &AtomicBool::new(false), &mut |_, _| {}, &|| {})
            .unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_tampered_runtime_is_never_unpacked() {
        let dir = TempDir::new("runtime-bad");
        let body = fake_release();
        let (url, _) = serve(body.clone(), Quirks::default());
        let asset: &'static PinnedAsset = Box::leak(Box::new(PinnedAsset {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            variant: "cpu",
            url: Box::leak(url.into_boxed_str()),
            sha256: Box::leak(sha(b"the real release").into_boxed_str()),
            size_bytes: body.len() as u64,
            archive: Archive::TarGz,
        }));
        let runtime = Runtime::with_asset(&dir.0, Some(asset));
        let r = runtime.ensure(
            &store::http_agent(true),
            &AtomicBool::new(false),
            &mut |_, _| {},
            &|| {},
        );
        assert!(
            matches!(&r, Err(PullError::Failed(why)) if why.contains("hash mismatch")),
            "{r:?}"
        );
        assert!(runtime.server_exe().is_none());
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
