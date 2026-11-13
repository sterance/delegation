//! Models on disk: `models/<model_id>/model.gguf` plus `manifest.json`,
//! under the client's data directory.
//!
//! A model only exists (and is offered) once its file has been downloaded,
//! checked against the size and SHA-256 the server sent, renamed into
//! place, and its manifest written. Anything else in the models directory
//! is ignored and logged. The model id is the only thing from the server
//! that ever becomes part of a path, and it's checked first.
//!
//! Downloads are resumable: they go to `<file>.partial`, which survives a
//! cancel, a failed connection or a client restart, and the next pull
//! continues it with a `Range` request.

use log::{debug, info, warn};
use protocol::{valid_model_id, Backend, ModelMeta, ModelSettings, ModelSpec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Every model's file has this name, whatever the server called it.
pub const MODEL_FILE: &str = "model.gguf";
const MANIFEST_FILE: &str = "manifest.json";
const MANIFEST_VERSION: u32 = 1;
/// Partial downloads nobody has resumed for this long are deleted at startup.
const STALE_PARTIAL_AGE: Duration = Duration::from_secs(86_400);
const READ_BUFFER: usize = 256 * 1024;
/// Hard limit on a model file, as a sanity check on what the server sends.
pub const MAX_MODEL_BYTES: u64 = 1 << 40;

/// Why a pull (or the runtime download within it) didn't finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullError {
    /// Stopped by a cancel. Partial files are kept, so pulling again resumes.
    Cancelled,
    /// A human-readable reason.
    Failed(String),
}

impl std::fmt::Display for PullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PullError::Cancelled => f.write_str("cancelled"),
            PullError::Failed(reason) => f.write_str(reason),
        }
    }
}

fn failed(reason: impl Into<String>) -> PullError {
    PullError::Failed(reason.into())
}

/// What `manifest.json` holds: everything needed to list and run a model
/// without asking the server again.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    pub id: String,
    pub display_name: String,
    pub backend: Backend,
    /// Always [`MODEL_FILE`]; recorded so a later layout can change it.
    pub file: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub source_url: String,
    pub context_length: Option<u32>,
    #[serde(default)]
    pub meta: ModelMeta,
    pub installed_at: String,
    /// From the dashboard (`configure`); kept across restarts and re-pulls.
    #[serde(default)]
    pub settings: ModelSettings,
}

/// The result of a successful pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOutcome {
    Downloaded,
    /// It was already there and verified; nothing was downloaded.
    AlreadyPresent,
}

/// Checks everything about a pull the client can check before starting.
pub fn validate_spec(spec: &ModelSpec, allow_http: bool) -> Result<(), String> {
    if !valid_model_id(&spec.id) {
        return Err(format!(
            "{:?} isn't a valid model id (lowercase letters, digits, '.', '_' and '-', at most {} characters)",
            spec.id,
            protocol::models::MODEL_ID_MAX_LEN
        ));
    }
    if spec.backend != Backend::LlamaCpp {
        return Err(format!("{:?} models are built in and can't be pulled", spec.backend));
    }
    let url = &spec.source.url;
    if !(url.starts_with("https://") || allow_http && url.starts_with("http://")) {
        return Err("model downloads must use HTTPS".into());
    }
    if !is_sha256_hex(&spec.source.sha256) {
        return Err("the model's SHA-256 must be 64 lowercase hex digits".into());
    }
    if spec.source.size_bytes == 0 || spec.source.size_bytes > MAX_MODEL_BYTES {
        return Err(format!(
            "the model's size ({} bytes) isn't plausible",
            spec.source.size_bytes
        ));
    }
    Ok(())
}

pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Free space on the volume holding a path; swapped out in tests.
pub type FreeSpaceFn = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

pub struct Store {
    models_dir: PathBuf,
    /// Verified models by id, read from their manifests at startup and kept
    /// up to date by pulls and deletes.
    manifests: Mutex<BTreeMap<String, Manifest>>,
    agent: ureq::Agent,
    allow_http: bool,
    free_space: FreeSpaceFn,
}

impl Store {
    /// Opens (creating if needed) `<data_dir>/models`, lists the models in
    /// it, and deletes partial downloads older than a day.
    pub fn open(data_dir: &Path) -> Store {
        Self::open_with(data_dir, false, Arc::new(crate::hardware::free_space))
    }

    /// `allow_http` lets tests download from a local plain-HTTP server.
    pub(crate) fn open_with(data_dir: &Path, allow_http: bool, free_space: FreeSpaceFn) -> Store {
        let models_dir = data_dir.join("models");
        if let Err(e) = std::fs::create_dir_all(&models_dir) {
            warn!(target: "models", "couldn't create {}: {e}", models_dir.display());
        }
        let store = Store {
            manifests: Mutex::new(scan(&models_dir)),
            models_dir,
            agent: http_agent(allow_http),
            allow_http,
            free_space,
        };
        remove_stale_partials(&store.models_dir, 2);
        store
    }

    pub fn allow_http(&self) -> bool {
        self.allow_http
    }

    pub(crate) fn agent(&self) -> &ureq::Agent {
        &self.agent
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Manifest>> {
        self.manifests.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every verified model, by id.
    pub fn list(&self) -> Vec<Manifest> {
        self.lock().values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Manifest> {
        self.lock().get(id).cloned()
    }

    /// The model's file. Only for ids that passed `valid_model_id`.
    pub fn model_path(&self, id: &str) -> PathBuf {
        self.models_dir.join(id).join(MODEL_FILE)
    }

    /// Bytes used under the models directory, partial downloads included.
    pub fn models_bytes(&self) -> u64 {
        dir_size(&self.models_dir, 2)
    }

    /// Free space on the models volume.
    pub fn free_bytes(&self) -> Option<u64> {
        (self.free_space)(&self.models_dir)
    }

    /// Downloads and verifies `spec` (blocking). `reserve_bytes` must stay
    /// free on the volume afterwards. Progress is (done, total) bytes.
    pub fn pull(
        &self,
        spec: &ModelSpec,
        reserve_bytes: u64,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<PullOutcome, PullError> {
        validate_spec(spec, self.allow_http).map_err(PullError::Failed)?;
        let size = spec.source.size_bytes;
        if let Some(existing) = self.get(&spec.id) {
            let file_ok = std::fs::metadata(self.model_path(&spec.id)).is_ok_and(|m| m.len() == existing.size_bytes);
            if existing.sha256 == spec.source.sha256 && existing.size_bytes == size && file_ok {
                info!(target: "models", "{} is already here and verified", spec.id);
                progress(size, size);
                return Ok(PullOutcome::AlreadyPresent);
            }
            info!(target: "models", "{} is here but differs from what was asked for; pulling it again", spec.id);
        }

        let dir = self.models_dir.join(&spec.id);
        std::fs::create_dir_all(&dir).map_err(|e| failed(format!("couldn't create {}: {e}", dir.display())))?;
        let file = dir.join(MODEL_FILE);
        let partial = dir.join(format!("{MODEL_FILE}.partial"));

        // Up front, so a pull that can't fit never starts.
        let have = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0).min(size);
        let needed = size - have;
        if let Some(free) = self.free_bytes() {
            if free.saturating_sub(reserve_bytes) < needed {
                return Err(failed(format!(
                    "not enough disk space: the model needs {} more, and {} is free with {} kept in reserve \
                     (INFERENCE_DISK_RESERVE_MB)",
                    mib(needed),
                    mib(free),
                    mib(reserve_bytes)
                )));
            }
        }

        info!(target: "models", "pulling {} ({}) from {}", spec.id, mib(size), spec.source.url);
        download(
            &self.agent,
            &Download {
                url: &spec.source.url,
                sha256: &spec.source.sha256,
                size,
                partial: &partial,
            },
            cancel,
            progress,
        )?;
        std::fs::rename(&partial, &file).map_err(|e| failed(format!("couldn't move the model into place: {e}")))?;
        let settings = self.get(&spec.id).map(|m| m.settings).unwrap_or_default();
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            id: spec.id.clone(),
            display_name: spec.display_name.clone(),
            backend: spec.backend,
            file: MODEL_FILE.into(),
            size_bytes: size,
            sha256: spec.source.sha256.clone(),
            source_url: spec.source.url.clone(),
            context_length: spec.context_length,
            meta: spec.meta.clone(),
            installed_at: protocol::ts(),
            settings,
        };
        let json = serde_json::to_vec_pretty(&manifest).map_err(|e| failed(e.to_string()))?;
        write_atomically(&dir.join(MANIFEST_FILE), &json)
            .map_err(|e| failed(format!("couldn't write the model's manifest: {e}")))?;
        self.lock().insert(spec.id.clone(), manifest);
        info!(target: "models", "{} is ready", spec.id);
        Ok(PullOutcome::Downloaded)
    }

    /// Stores new settings in the model's manifest. Returns false if there's
    /// no such model.
    pub fn configure(&self, id: &str, settings: ModelSettings) -> std::io::Result<bool> {
        let Some(mut manifest) = self.get(id) else {
            return Ok(false);
        };
        manifest.settings = settings;
        let json = serde_json::to_vec_pretty(&manifest).map_err(std::io::Error::other)?;
        write_atomically(&self.models_dir.join(id).join(MANIFEST_FILE), &json)?;
        self.lock().insert(id.to_string(), manifest);
        Ok(true)
    }

    /// Removes the model's directory, partial download included. Returns
    /// whether there was anything to remove. The caller has unloaded it.
    pub fn delete(&self, id: &str) -> std::io::Result<bool> {
        if !valid_model_id(id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid model id",
            ));
        }
        let had = self.lock().remove(id).is_some();
        let dir = self.models_dir.join(id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {
                info!(target: "models", "deleted {id}");
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(had),
            Err(e) => Err(e),
        }
    }
}

/// The verified models in `models_dir`, logging (and skipping) anything
/// that isn't one.
fn scan(models_dir: &Path) -> BTreeMap<String, Manifest> {
    let mut found = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(models_dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match read_manifest(&path, &name) {
            Ok(manifest) => {
                debug!(target: "models", "found {name}");
                found.insert(name, manifest);
            }
            Err(why) => info!(target: "models", "ignoring {}: {why}", path.display()),
        }
    }
    info!(target: "models", "{} model(s) in {}", found.len(), models_dir.display());
    found
}

fn read_manifest(dir: &Path, name: &str) -> Result<Manifest, String> {
    if !valid_model_id(name) {
        return Err("not a valid model id".into());
    }
    let text = match std::fs::read_to_string(dir.join(MANIFEST_FILE)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("no manifest (an unfinished download?)".into())
        }
        Err(e) => return Err(format!("unreadable manifest: {e}")),
    };
    let manifest: Manifest = serde_json::from_str(&text).map_err(|e| format!("invalid manifest: {e}"))?;
    if manifest.id != name || manifest.file != MODEL_FILE || !is_sha256_hex(&manifest.sha256) {
        return Err("the manifest doesn't describe this directory".into());
    }
    let len = std::fs::metadata(dir.join(MODEL_FILE))
        .map(|m| m.len())
        .map_err(|e| format!("model file missing: {e}"))?;
    if len != manifest.size_bytes {
        return Err(format!(
            "model file is {len} bytes, the manifest says {}",
            manifest.size_bytes
        ));
    }
    Ok(manifest)
}

/// Deletes `*.partial` files not touched for a day, `depth` levels down.
pub(crate) fn remove_stale_partials(dir: &Path, depth: u32) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if depth > 1 {
                remove_stale_partials(&path, depth - 1);
            }
            continue;
        }
        let is_partial = path.extension().is_some_and(|e| e == "partial");
        let age = meta
            .modified()
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .unwrap_or_default();
        if is_partial && age >= STALE_PARTIAL_AGE {
            match std::fs::remove_file(&path) {
                Ok(()) => info!(target: "models", "removed stale partial download {}", path.display()),
                Err(e) => warn!(target: "models", "couldn't remove {}: {e}", path.display()),
            }
        }
    }
}

fn dir_size(dir: &Path, depth: u32) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(m) if m.is_dir() && depth > 1 => dir_size(&entry.path(), depth - 1),
            Ok(m) if m.is_file() => m.len(),
            _ => 0,
        })
        .sum()
}

/// Writes a temp file next to `path`, flushes it, and renames it over
/// `path`, so a crash never leaves a half-written file behind.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

pub(crate) fn mib(bytes: u64) -> String {
    format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// The agent every download uses. HTTPS only (on every redirect hop too),
/// except in tests.
pub(crate) fn http_agent(allow_http: bool) -> ureq::Agent {
    #[allow(unused_mut)]
    let mut builder = ureq::AgentBuilder::new()
        .https_only(!allow_http)
        .redirects(10)
        .timeout_connect(Duration::from_secs(30))
        // Per read, not for the whole download.
        .timeout_read(Duration::from_secs(60))
        .user_agent(concat!("delegation-client/", env!("CARGO_PKG_VERSION")));
    // ureq only uses native-tls when handed a connector explicitly.
    #[cfg(windows)]
    if let Ok(tls) = native_tls::TlsConnector::new() {
        builder = builder.tls_connector(Arc::new(tls));
    }
    builder.build()
}

/// One file to fetch and verify.
pub(crate) struct Download<'a> {
    pub url: &'a str,
    /// Lowercase hex.
    pub sha256: &'a str,
    pub size: u64,
    /// Where the bytes go. Kept on cancel or a broken connection, so the
    /// next attempt resumes; deleted if what arrived is wrong.
    pub partial: &'a Path,
}

/// Fetches `d.url` into `d.partial`, resuming whatever is already there,
/// and checks the size and SHA-256 of the whole file. Blocking. On success
/// the partial file is complete and verified; the caller moves it.
pub(crate) fn download(
    agent: &ureq::Agent,
    d: &Download<'_>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), PullError> {
    let mut have = std::fs::metadata(d.partial).map(|m| m.len()).unwrap_or(0);
    if have > d.size {
        info!(target: "download", "{} is larger than expected, starting over", d.partial.display());
        remove_quietly(d.partial);
        have = 0;
    }
    let mut hasher = Sha256::new();
    if have > 0 {
        // The hash covers the whole file, so the part already here is read
        // back first.
        hash_existing(d.partial, have, &mut hasher, cancel)?;
        info!(target: "download", "resuming {} at {} of {}", d.url, mib(have), mib(d.size));
    }
    progress(have, d.size);

    // Twice at most: once more from scratch if the server refuses to resume.
    let mut attempts = 0;
    while have < d.size {
        attempts += 1;
        let mut request = agent.get(d.url);
        if have > 0 {
            request = request.set("Range", &format!("bytes={have}-"));
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(416, _)) if have > 0 && attempts == 1 => {
                info!(target: "download", "the server can't resume {}, starting over", d.url);
                remove_quietly(d.partial);
                have = 0;
                hasher = Sha256::new();
                continue;
            }
            Err(ureq::Error::Status(code, _)) => {
                return Err(failed(format!("the download server answered HTTP {code}")))
            }
            Err(e) => return Err(failed(format!("the download failed: {e}"))),
        };
        let mut file = match (have, response.status()) {
            (0, 200) => File::create(d.partial),
            (_, 206) if content_range_starts_at(response.header("content-range"), have) => {
                OpenOptions::new().append(true).open(d.partial)
            }
            (_, 200) => {
                info!(target: "download", "the server ignored the resume request for {}, starting over", d.url);
                have = 0;
                hasher = Sha256::new();
                File::create(d.partial)
            }
            (_, status) => return Err(failed(format!("unexpected response to the download: HTTP {status}"))),
        }
        .map_err(|e| failed(format!("couldn't write {}: {e}", d.partial.display())))?;

        let mut body = response.into_reader();
        let mut buf = vec![0u8; READ_BUFFER];
        loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = file.sync_all();
                return Err(PullError::Cancelled);
            }
            let n = match body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // Kept for resuming: what arrived so far is still good.
                Err(e) => {
                    let _ = file.sync_all();
                    return Err(failed(format!("the download broke off at {}: {e}", mib(have))));
                }
            };
            if have + n as u64 > d.size {
                drop(file);
                remove_quietly(d.partial);
                return Err(failed(format!(
                    "size mismatch: the server sent more than the expected {} bytes",
                    d.size
                )));
            }
            file.write_all(&buf[..n])
                .map_err(|e| failed(format!("couldn't write {}: {e}", d.partial.display())))?;
            hasher.update(&buf[..n]);
            have += n as u64;
            progress(have, d.size);
        }
        file.sync_all()
            .map_err(|e| failed(format!("couldn't write {}: {e}", d.partial.display())))?;
        if have < d.size {
            remove_quietly(d.partial);
            return Err(failed(format!(
                "size mismatch: the download ended at {have} bytes, expected {}",
                d.size
            )));
        }
    }

    let actual = hex::encode(hasher.finalize());
    if actual != d.sha256 {
        remove_quietly(d.partial);
        return Err(failed(format!(
            "hash mismatch: expected SHA-256 {}, got {actual}",
            d.sha256
        )));
    }
    Ok(())
}

fn hash_existing(path: &Path, len: u64, hasher: &mut Sha256, cancel: &AtomicBool) -> Result<(), PullError> {
    let io = |e: std::io::Error| failed(format!("couldn't read {}: {e}", path.display()));
    let mut file = File::open(path).map_err(io)?;
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut reader = file.take(len);
    let mut buf = vec![0u8; READ_BUFFER];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(PullError::Cancelled);
        }
        let n = reader.read(&mut buf).map_err(io)?;
        if n == 0 {
            return Ok(());
        }
        hasher.update(&buf[..n]);
    }
}

/// `Content-Range: bytes <start>-<end>/<total>` starts where we asked.
fn content_range_starts_at(header: Option<&str>, start: u64) -> bool {
    header
        .and_then(|h| h.trim().strip_prefix("bytes "))
        .and_then(|r| r.split('-').next())
        .and_then(|s| s.trim().parse::<u64>().ok())
        == Some(start)
}

fn remove_quietly(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(target: "download", "couldn't remove {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use protocol::ModelSource;
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;

    /// A throwaway directory, removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(name: &str) -> TempDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("delegation-{name}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// How the test server misbehaves.
    #[derive(Clone, Copy, Default)]
    pub struct Quirks {
        /// Close the connection after this many body bytes (of a 200).
        pub cut_after: Option<usize>,
        /// Answer Range requests with the whole file.
        pub ignore_range: bool,
        /// Send 64 KiB every 20 ms, so a test can act mid-download.
        pub slow: bool,
    }

    /// A plain-HTTP server for `body` that honours `Range: bytes=N-`.
    /// Returns its URL and a counter of requests served.
    pub fn serve(body: Vec<u8>, quirks: Quirks) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model.gguf", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut range_start = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("range: bytes=") {
                        range_start = v.trim().trim_end_matches('-').parse::<usize>().ok();
                    }
                }
                let (status, slice, extra) = match range_start {
                    Some(start) if !quirks.ignore_range && start < body.len() => (
                        "206 Partial Content",
                        &body[start..],
                        format!("Content-Range: bytes {start}-{}/{}\r\n", body.len() - 1, body.len()),
                    ),
                    Some(start) if !quirks.ignore_range && start >= body.len() => {
                        let _ = stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\n\r\n");
                        continue;
                    }
                    _ => ("200 OK", &body[..], String::new()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                    slice.len()
                );
                let _ = stream.write_all(head.as_bytes());
                match quirks.cut_after {
                    Some(n) if status.starts_with("200") && n < slice.len() => {
                        let _ = stream.write_all(&slice[..n]);
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                    _ if quirks.slow => {
                        for piece in slice.chunks(64 * 1024) {
                            if stream.write_all(piece).is_err() {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                    _ => {
                        let _ = stream.write_all(slice);
                    }
                }
            }
        });
        (url, requests)
    }

    pub fn blob(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    pub fn sha(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    pub fn spec(id: &str, url: &str, body: &[u8]) -> ModelSpec {
        ModelSpec {
            id: id.into(),
            display_name: format!("Model {id}"),
            backend: Backend::LlamaCpp,
            source: ModelSource {
                url: url.into(),
                sha256: sha(body),
                size_bytes: body.len() as u64,
            },
            context_length: Some(2048),
            meta: ModelMeta::default(),
            min_runtime_version: None,
        }
    }

    fn store_in(dir: &Path, free: Option<u64>) -> Store {
        Store::open_with(dir, true, Arc::new(move |_| free))
    }

    fn pull(store: &Store, spec: &ModelSpec) -> Result<PullOutcome, PullError> {
        store.pull(spec, 0, &AtomicBool::new(false), &mut |_, _| {})
    }

    #[test]
    fn a_pull_downloads_verifies_and_lists_the_model() {
        let dir = TempDir::new("pull-ok");
        let body = blob(600_000);
        let (url, _) = serve(body.clone(), Quirks::default());
        let store = store_in(&dir.0, None);
        let spec = spec("m1", &url, &body);
        let mut reports = Vec::new();
        let outcome = store.pull(&spec, 0, &AtomicBool::new(false), &mut |done, total| {
            reports.push((done, total))
        });
        assert_eq!(outcome, Ok(PullOutcome::Downloaded));
        assert_eq!(std::fs::read(store.model_path("m1")).unwrap(), body);
        assert!(!dir.0.join("models/m1/model.gguf.partial").exists());
        assert_eq!(reports.first(), Some(&(0, 600_000)));
        assert_eq!(reports.last(), Some(&(600_000, 600_000)));
        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].display_name, "Model m1");
        // A fresh start finds it from its manifest.
        let reopened = store_in(&dir.0, None);
        assert_eq!(reopened.get("m1").unwrap().sha256, spec.source.sha256);
        assert_eq!(
            reopened.models_bytes(),
            600_000 + std::fs::metadata(dir.0.join("models/m1/manifest.json")).unwrap().len()
        );
    }

    #[test]
    fn pulling_a_present_model_again_downloads_nothing() {
        let dir = TempDir::new("pull-again");
        let body = blob(10_000);
        let (url, requests) = serve(body.clone(), Quirks::default());
        let store = store_in(&dir.0, None);
        let spec = spec("m1", &url, &body);
        assert_eq!(pull(&store, &spec), Ok(PullOutcome::Downloaded));
        assert_eq!(pull(&store, &spec), Ok(PullOutcome::AlreadyPresent));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_interrupted_download_resumes_where_it_stopped() {
        let dir = TempDir::new("pull-resume");
        let body = blob(500_000);
        // The first response breaks off after 200 KB; Range requests work.
        let (url, requests) = serve(
            body.clone(),
            Quirks {
                cut_after: Some(200_000),
                ..Quirks::default()
            },
        );
        let store = store_in(&dir.0, None);
        let spec = spec("m1", &url, &body);
        let first = pull(&store, &spec);
        assert!(
            matches!(&first, Err(PullError::Failed(why)) if why.contains("broke off") || why.contains("size mismatch")),
            "{first:?}"
        );
        let partial = dir.0.join("models/m1/model.gguf.partial");
        // A clean early end (no error from the reader) deletes the file as
        // a size mismatch; a broken connection keeps it. Make sure we tested
        // the resume path: put back the first 200 KB if it was deleted.
        if !partial.exists() {
            std::fs::write(&partial, &body[..200_000]).unwrap();
        }
        assert!(std::fs::metadata(&partial).unwrap().len() <= 200_000);
        let mut first_report = None;
        let outcome = store.pull(&spec, 0, &AtomicBool::new(false), &mut |done, _| {
            first_report.get_or_insert(done);
        });
        assert_eq!(outcome, Ok(PullOutcome::Downloaded));
        assert!(first_report.unwrap() > 0, "progress started from the partial file");
        assert_eq!(std::fs::read(store.model_path("m1")).unwrap(), body);
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_server_that_ignores_range_restarts_cleanly() {
        let dir = TempDir::new("pull-norange");
        let body = blob(300_000);
        let (url, _) = serve(
            body.clone(),
            Quirks {
                ignore_range: true,
                ..Quirks::default()
            },
        );
        let store = store_in(&dir.0, None);
        std::fs::create_dir_all(dir.0.join("models/m1")).unwrap();
        std::fs::write(dir.0.join("models/m1/model.gguf.partial"), &body[..1000]).unwrap();
        assert_eq!(pull(&store, &spec("m1", &url, &body)), Ok(PullOutcome::Downloaded));
        assert_eq!(std::fs::read(store.model_path("m1")).unwrap(), body);
    }

    #[test]
    fn size_and_hash_mismatches_fail_and_delete_the_file() {
        let dir = TempDir::new("pull-mismatch");
        let body = blob(50_000);
        let (url, _) = serve(body.clone(), Quirks::default());
        let store = store_in(&dir.0, None);

        let mut wrong_hash = spec("m1", &url, &body);
        wrong_hash.source.sha256 = sha(b"something else");
        let r = pull(&store, &wrong_hash);
        assert!(
            matches!(&r, Err(PullError::Failed(why)) if why.contains("hash mismatch")),
            "{r:?}"
        );
        assert!(!dir.0.join("models/m1/model.gguf.partial").exists());
        assert!(store.get("m1").is_none());

        let mut too_small = spec("m2", &url, &body);
        too_small.source.size_bytes = 40_000;
        let r = pull(&store, &too_small);
        assert!(
            matches!(&r, Err(PullError::Failed(why)) if why.contains("size mismatch")),
            "{r:?}"
        );
        assert!(!dir.0.join("models/m2/model.gguf.partial").exists());

        let mut too_big = spec("m3", &url, &body);
        too_big.source.size_bytes = 60_000;
        let r = pull(&store, &too_big);
        assert!(
            matches!(&r, Err(PullError::Failed(why)) if why.contains("size mismatch")),
            "{r:?}"
        );
        assert!(store.list().is_empty());
    }

    #[test]
    fn a_cancelled_pull_keeps_its_partial_file() {
        let dir = TempDir::new("pull-cancel");
        let body = blob(2_000_000);
        let (url, _) = serve(body.clone(), Quirks::default());
        let store = store_in(&dir.0, None);
        let cancel = AtomicBool::new(false);
        let r = store.pull(&spec("m1", &url, &body), 0, &cancel, &mut |done, _| {
            if done > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!(r, Err(PullError::Cancelled));
        let partial = std::fs::metadata(dir.0.join("models/m1/model.gguf.partial"))
            .unwrap()
            .len();
        assert!(partial > 0 && partial < 2_000_000, "{partial}");
        assert!(store.get("m1").is_none());
        // And pulling again finishes it.
        assert_eq!(pull(&store, &spec("m1", &url, &body)), Ok(PullOutcome::Downloaded));
        assert_eq!(std::fs::read(store.model_path("m1")).unwrap(), body);
    }

    #[test]
    fn a_pull_that_would_eat_the_reserve_is_refused_up_front() {
        let dir = TempDir::new("pull-disk");
        let body = blob(10_000);
        let (url, requests) = serve(body.clone(), Quirks::default());
        let store = store_in(&dir.0, Some(15_000));
        let r = store.pull(&spec("m1", &url, &body), 8_000, &AtomicBool::new(false), &mut |_, _| {});
        assert!(
            matches!(&r, Err(PullError::Failed(why)) if why.contains("not enough disk space")),
            "{r:?}"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 0, "nothing was downloaded");
        assert_eq!(
            store.pull(&spec("m1", &url, &body), 4_000, &AtomicBool::new(false), &mut |_, _| {}),
            Ok(PullOutcome::Downloaded)
        );
    }

    #[test]
    fn bad_ids_and_sources_are_rejected_before_anything_touches_the_disk() {
        let dir = TempDir::new("pull-ids");
        let store = Store::open_with(&dir.0, false, Arc::new(|_| None));
        let body = blob(10);
        for id in ["../escape", "..", "a/b", "UPPER", "", "x\\y", "/abs"] {
            let r = pull(&store, &spec(id, "https://example.com/m", &body));
            assert!(
                matches!(&r, Err(PullError::Failed(why)) if why.contains("valid model id")),
                "{id:?}: {r:?}"
            );
        }
        let r = pull(&store, &spec("m1", "http://example.com/m", &body));
        assert!(
            matches!(&r, Err(PullError::Failed(why)) if why.contains("HTTPS")),
            "{r:?}"
        );
        let mut bad_hash = spec("m1", "https://example.com/m", &body);
        bad_hash.source.sha256 = "ABC".into();
        assert!(pull(&store, &bad_hash).is_err());
        let mut echo = spec("m1", "https://example.com/m", &body);
        echo.backend = Backend::Echo;
        assert!(pull(&store, &echo).is_err());
        assert!(store.delete("../models").is_err());
        assert_eq!(std::fs::read_dir(dir.0.join("models")).unwrap().count(), 0);
        assert!(dir.0.parent().unwrap().exists(), "nothing outside was touched");
    }

    #[test]
    fn startup_ignores_broken_directories_and_old_partials() {
        let dir = TempDir::new("scan");
        let models = dir.0.join("models");
        let body = blob(1000);
        let (url, _) = serve(body.clone(), Quirks::default());
        let store = store_in(&dir.0, None);
        pull(&store, &spec("good", &url, &body)).unwrap();
        // No manifest, a manifest for another id, a truncated file, a bad name.
        std::fs::create_dir_all(models.join("half")).unwrap();
        std::fs::write(models.join("half/model.gguf.partial"), b"abc").unwrap();
        std::fs::create_dir_all(models.join("other")).unwrap();
        std::fs::copy(models.join("good/manifest.json"), models.join("other/manifest.json")).unwrap();
        std::fs::copy(models.join("good/model.gguf"), models.join("other/model.gguf")).unwrap();
        std::fs::create_dir_all(models.join("short")).unwrap();
        let manifest = std::fs::read_to_string(models.join("good/manifest.json")).unwrap();
        std::fs::write(
            models.join("short/manifest.json"),
            manifest.replace("\"good\"", "\"short\""),
        )
        .unwrap();
        std::fs::write(models.join("short/model.gguf"), b"tiny").unwrap();
        std::fs::create_dir_all(models.join("Bad Name")).unwrap();
        let old = File::options()
            .write(true)
            .open(models.join("half/model.gguf.partial"))
            .unwrap();
        old.set_modified(SystemTime::now() - Duration::from_secs(2 * 86_400))
            .unwrap();
        drop(old);

        let reopened = store_in(&dir.0, None);
        let ids: Vec<String> = reopened.list().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["good"]);
        assert!(
            !models.join("half/model.gguf.partial").exists(),
            "the day-old partial was removed"
        );

        assert!(reopened.delete("good").unwrap());
        assert!(!reopened.delete("good").unwrap(), "deleting a missing model is fine");
        assert!(reopened.list().is_empty());
    }

    #[test]
    fn settings_survive_a_restart_and_a_pull_again() {
        let dir = TempDir::new("configure");
        let body = blob(1000);
        let (url, _) = serve(body.clone(), Quirks::default());
        let store = store_in(&dir.0, None);
        pull(&store, &spec("m1", &url, &body)).unwrap();
        let settings = ModelSettings {
            gpu_layers: protocol::GpuLayers::Count(7),
            context_length: Some(8192),
        };
        assert!(store.configure("m1", settings).unwrap());
        assert!(!store.configure("nope", settings).unwrap());
        assert_eq!(store_in(&dir.0, None).get("m1").unwrap().settings, settings);
        // A changed file is pulled again; its settings stay.
        let other = blob(1001);
        let (url2, _) = serve(other.clone(), Quirks::default());
        pull(&store, &spec("m1", &url2, &other)).unwrap();
        assert_eq!(store.get("m1").unwrap().settings, settings);
    }

    #[test]
    fn content_range_is_checked() {
        assert!(content_range_starts_at(Some("bytes 100-199/200"), 100));
        assert!(!content_range_starts_at(Some("bytes 0-199/200"), 100));
        assert!(!content_range_starts_at(None, 100));
    }
}
