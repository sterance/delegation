//! The llama.cpp backend: one managed `llama-server` process with one
//! model loaded at a time, and chat streaming through its OpenAI-compatible
//! `/v1/chat/completions` (so the model's own chat template is applied).
//!
//! - It listens on 127.0.0.1, on a port picked by the client, and wants a
//!   random per-process API key (passed in its environment rather than on
//!   the command line, where other users could read it), so other local
//!   programs can't use it.
//! - A request for a model that isn't loaded swaps the process for one
//!   with that model, unless the loaded one is mid-reply ("busy with
//!   another model").
//! - It's stopped after `idle_unload_secs` without requests, giving the
//!   memory back to the volunteer.
//! - It never outlives the client: `PR_SET_PDEATHSIG` on Linux, a
//!   kill-on-close job object on Windows (see `process.rs`), and a kill on
//!   drop and on clean shutdown everywhere.
//! - It runs on the runtime variant `runtime.rs` resolves (CPU, CUDA or
//!   Vulkan). GPU layers follow the model's settings; by default llama.cpp
//!   fits as many as the free VRAM allows (`--fit`). If a GPU variant
//!   fails to load the model, that variant is marked failed and the load
//!   is retried once on the next candidate.
//! - A reply streams over its own local HTTP connection. Dropping the
//!   `chat` future drops the socket, and llama-server stops generating
//!   when its client goes away. That's why this talks HTTP/1.1 by hand over
//!   a tokio socket instead of using the (blocking) ureq: a blocked read
//!   would only notice the cancel after the next token.

use super::process;
use super::store::Manifest;
use super::{ChatCompletion, InferenceError, InferenceEvent, Shared};
use log::{debug, info, warn};
use protocol::models::MIN_CONTEXT;
use protocol::{GpuLayers, GpuOffload, InferenceSettings, RunInfo, Stage, Usage};
use rand::RngExt as _;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Lines of llama-server output kept for error reports.
const TAIL_LINES: usize = 50;
/// The most of that tail an error report carries.
const TAIL_MAX_CHARS: usize = 4_000;
const HEALTH_POLL: Duration = Duration::from_millis(250);
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
const STOP_GRACE: Duration = Duration::from_secs(5);

/// What the inventory reports about the process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcStatus {
    Stopped,
    Loading(String),
    Loaded(String),
}

pub struct LlamaCpp {
    proc: tokio::sync::Mutex<Option<Running>>,
    status: Mutex<ProcStatus>,
    /// Replies being generated, whatever the model.
    active: Arc<AtomicUsize>,
    next_generation: AtomicU64,
    /// Where the loaded model runs: (model id, run info).
    run: Mutex<Option<(String, RunInfo)>>,
}

impl Default for LlamaCpp {
    fn default() -> Self {
        LlamaCpp {
            proc: tokio::sync::Mutex::new(None),
            status: Mutex::new(ProcStatus::Stopped),
            active: Arc::new(AtomicUsize::new(0)),
            next_generation: AtomicU64::new(1),
            run: Mutex::new(None),
        }
    }
}

impl LlamaCpp {
    pub fn status(&self) -> ProcStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// What the loaded model runs on, if `model_id` is the loaded one.
    pub fn run_info(&self, model_id: &str) -> Option<RunInfo> {
        let run = self.run.lock().unwrap_or_else(|e| e.into_inner());
        run.as_ref().filter(|(id, _)| id == model_id).map(|(_, r)| r.clone())
    }

    fn set_status(&self, shared: &Shared, status: ProcStatus) {
        if !matches!(status, ProcStatus::Loaded(_)) {
            *self.run.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        let mut current = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if *current != status {
            *current = status;
            drop(current);
            shared.changed();
        }
    }
}

/// The last lines llama-server printed.
#[derive(Clone, Default)]
struct Tail(Arc<Mutex<VecDeque<String>>>);

impl Tail {
    fn push(&self, line: String) {
        let mut lines = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if lines.len() == TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// The kept lines, trimmed from the front to `TAIL_MAX_CHARS`.
    fn report(&self) -> String {
        let lines = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut text = lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        if text.len() > TAIL_MAX_CHARS {
            let mut cut = text.len() - TAIL_MAX_CHARS;
            while !text.is_char_boundary(cut) {
                cut += 1;
            }
            text = format!("…{}", &text[cut..]);
        }
        if text.is_empty() {
            "(it printed nothing)".into()
        } else {
            text
        }
    }
}

/// What llama-server's log says about GPU offload. In b12110, while
/// loading: "llama_model_load_from_file_impl: using device CUDA0 (NVIDIA
/// GeForce RTX 3070) (0000:01:00.0) - 7347 MiB free" for each GPU it uses,
/// then "load_tensors: offloaded 29/29 layers to GPU". `--fit` may load
/// more than once, so the last summary wins.
#[derive(Debug, Clone, Default, PartialEq)]
struct OffloadLog {
    devices: Vec<String>,
    layers: Option<GpuOffload>,
}

impl OffloadLog {
    fn push(&mut self, line: &str) {
        if let Some(layers) = parse_offloaded(line) {
            self.layers = Some(layers);
        } else if let Some(device) = parse_using_device(line) {
            if !self.devices.contains(&device) {
                self.devices.push(device);
            }
        }
    }

    fn run_info(&self, variant: &str) -> RunInfo {
        let on_gpu = self.layers.is_some_and(|l| l.offloaded > 0);
        RunInfo {
            variant: variant.to_string(),
            gpu_layers: self.layers,
            device: (on_gpu && !self.devices.is_empty()).then(|| self.devices.join(", ")),
        }
    }
}

/// The llama.cpp log formats `parse_offloaded` and `parse_using_device`
/// read, as they appear in `libllama`. The runtime-smoke CI job checks a
/// new release still has them (`smoke.rs`).
#[cfg(test)]
pub(super) const OFFLOADED_FORMAT: &str = "offloaded %d/%d layers to GPU";
#[cfg(test)]
pub(super) const USING_DEVICE_FORMAT: &str = "using device %s (%s) (%s) - %zu MiB free";

/// "…: offloaded 29/29 layers to GPU"
pub(super) fn parse_offloaded(line: &str) -> Option<GpuOffload> {
    let rest = &line[line.find("offloaded ")? + "offloaded ".len()..];
    let (counts, tail) = rest.split_once(' ')?;
    if !tail.starts_with("layers to GPU") {
        return None;
    }
    let (offloaded, total) = counts.split_once('/')?;
    Some(GpuOffload {
        offloaded: offloaded.parse().ok()?,
        total: total.parse().ok()?,
    })
}

/// "…: using device CUDA0 (NVIDIA GeForce RTX 3070) (0000:01:00.0) - 7347 MiB free"
/// -> "NVIDIA GeForce RTX 3070"
pub(super) fn parse_using_device(line: &str) -> Option<String> {
    let rest = &line[line.find("using device ")? + "using device ".len()..];
    let rest = &rest[rest.find(" (")? + 2..];
    let rest = &rest[..rest.rfind(" - ")?];
    let description = &rest[..rest.rfind(") (")?];
    Some(description.to_string()).filter(|d| !d.is_empty())
}

struct Running {
    generation: u64,
    model_id: String,
    run: RunInfo,
    child: Child,
    addr: SocketAddr,
    api_key: String,
    tail: Tail,
    /// Requests using this process right now.
    users: Arc<AtomicUsize>,
    last_used: Arc<Mutex<Instant>>,
}

impl Running {
    fn lease(&self) -> Lease {
        self.users.fetch_add(1, Ordering::SeqCst);
        *self.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        Lease {
            generation: self.generation,
            run: self.run.clone(),
            addr: self.addr,
            api_key: self.api_key.clone(),
            users: self.users.clone(),
            last_used: self.last_used.clone(),
        }
    }

    /// Why it exited, if it has.
    fn exited(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            Ok(None) => None,
            Err(e) => Some(format!("unknown status: {e}")),
        }
    }

    async fn stop(mut self) {
        info!(target: "llama", "stopping llama-server ({})", self.model_id);
        let _ = self.child.start_kill();
        if tokio::time::timeout(STOP_GRACE, self.child.wait()).await.is_err() {
            warn!(target: "llama", "llama-server didn't exit within {STOP_GRACE:?}");
        }
    }
}

/// A request's hold on the process: it isn't idle (or swapped) while held.
struct Lease {
    generation: u64,
    run: RunInfo,
    addr: SocketAddr,
    api_key: String,
    users: Arc<AtomicUsize>,
    last_used: Arc<Mutex<Instant>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        *self.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        self.users.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One of `max_concurrent` places to generate.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(active: &Arc<AtomicUsize>, max: u32) -> Option<Slot> {
        active
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < max as usize).then_some(n + 1)
            })
            .ok()?;
        Some(Slot(active.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Puts the status back to `Stopped` if a load is abandoned (the request
/// that started it was cancelled).
struct LoadingGuard<'a> {
    shared: &'a Shared,
    armed: bool,
}

impl Drop for LoadingGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.shared.llama.set_status(self.shared, ProcStatus::Stopped);
        }
    }
}

/// Streams a reply from `model`, loading it first if needed.
pub(crate) async fn chat(
    shared: &Arc<Shared>,
    model: &Manifest,
    req: &ChatCompletion,
    max_tokens: u32,
    out: &mpsc::Sender<InferenceEvent>,
) -> Result<Option<Usage>, InferenceError> {
    let settings = shared.settings();
    let _slot = Slot::take(&shared.llama.active, settings.max_concurrent).ok_or(InferenceError::Busy)?;
    let lease = acquire(shared, model, &settings, out).await?;
    out.send(InferenceEvent::Status(Stage::Generating))
        .await
        .map_err(|_| InferenceError::Closed)?;
    let body = request_body(&model.id, req, max_tokens);
    match stream_chat(lease.addr, &lease.api_key, &body, out).await {
        Ok(usage) => Ok(usage.map(|u| Usage {
            run: Some(lease.run.clone()),
            ..u
        })),
        Err(StreamError::Closed) => Err(InferenceError::Closed),
        Err(StreamError::Backend(message)) => Err(InferenceError::Backend(message)),
        Err(StreamError::Io(e)) => match crashed(shared, lease.generation).await {
            Some(report) => Err(InferenceError::Crashed(report)),
            None => Err(InferenceError::Backend(format!(
                "lost the connection to llama-server: {e}"
            ))),
        },
    }
}

/// A lease on a process running `model`, starting (or swapping) one if needed.
async fn acquire(
    shared: &Arc<Shared>,
    model: &Manifest,
    settings: &InferenceSettings,
    out: &mpsc::Sender<InferenceEvent>,
) -> Result<Lease, InferenceError> {
    let llama = &shared.llama;
    let mut proc = llama.proc.lock().await;
    if let Some(running) = proc.as_mut() {
        if let Some(status) = running.exited() {
            warn!(
                target: "llama",
                "llama-server exited on its own ({status}); its last output:\n{}",
                running.tail.report()
            );
            *proc = None;
            llama.set_status(shared, ProcStatus::Stopped);
        }
    }
    if let Some(running) = proc.as_ref() {
        if running.model_id == model.id {
            return Ok(running.lease());
        }
        if running.users.load(Ordering::SeqCst) > 0 {
            return Err(InferenceError::BusyWithAnotherModel(running.model_id.clone()));
        }
        info!(target: "llama", "switching from {} to {}", running.model_id, model.id);
        if let Some(old) = proc.take() {
            old.stop().await;
        }
        llama.set_status(shared, ProcStatus::Stopped);
    }

    out.send(InferenceEvent::Status(Stage::LoadingModel))
        .await
        .map_err(|_| InferenceError::Closed)?;
    llama.set_status(shared, ProcStatus::Loading(model.id.clone()));
    let mut guard = LoadingGuard { shared, armed: true };
    let running = match start(shared, model, settings).await {
        Ok(running) => running,
        // A GPU build that can't load it: that variant is out, and the
        // load is tried once more on the next candidate.
        Err(StartError {
            variant: Some(variant),
            why,
        }) if variant != "cpu" => {
            shared.runtime.mark_failed(&variant, &why);
            shared.changed();
            warn!(target: "llama", "retrying the load of {} without llama.cpp {variant}", model.id);
            start(shared, model, settings)
                .await
                .map_err(|e| InferenceError::LoadFailed(e.why))?
        }
        Err(e) => return Err(InferenceError::LoadFailed(e.why)),
    };
    guard.armed = false;
    let lease = running.lease();
    let generation = running.generation;
    *llama.run.lock().unwrap_or_else(|e| e.into_inner()) = Some((model.id.clone(), running.run.clone()));
    *proc = Some(running);
    llama.set_status(shared, ProcStatus::Loaded(model.id.clone()));
    tokio::spawn(watch(shared.clone(), generation));
    Ok(lease)
}

/// If the process behind `generation` has died, clears it and returns a
/// report for the error.
async fn crashed(shared: &Arc<Shared>, generation: u64) -> Option<String> {
    let mut proc = shared.llama.proc.lock().await;
    let running = proc.as_mut().filter(|r| r.generation == generation)?;
    let status = running.exited()?;
    let report = format!("it exited ({status}). Its last output:\n{}", running.tail.report());
    warn!(target: "llama", "llama-server crashed: {report}");
    *proc = None;
    shared.llama.set_status(shared, ProcStatus::Stopped);
    Some(report)
}

/// Per process: notices it exiting on its own, and stops it once it's been
/// idle for `idle_unload_secs`. Ends when the process does.
async fn watch(shared: Arc<Shared>, generation: u64) {
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        let mut proc = shared.llama.proc.lock().await;
        let Some(running) = proc.as_mut().filter(|r| r.generation == generation) else {
            return;
        };
        if let Some(status) = running.exited() {
            warn!(
                target: "llama",
                "llama-server exited on its own ({status}); its last output:\n{}",
                running.tail.report()
            );
            *proc = None;
            shared.llama.set_status(&shared, ProcStatus::Stopped);
            return;
        }
        let idle_limit = shared.settings().idle_unload_secs;
        let idle_for = running.last_used.lock().unwrap_or_else(|e| e.into_inner()).elapsed();
        if idle_limit > 0 && running.users.load(Ordering::SeqCst) == 0 && idle_for.as_secs() >= idle_limit {
            info!(target: "llama", "idle for {} s, unloading {}", idle_for.as_secs(), running.model_id);
            if let Some(running) = proc.take() {
                running.stop().await;
            }
            shared.llama.set_status(&shared, ProcStatus::Stopped);
            return;
        }
    }
}

/// Stops the process if it's running `model_id` (before deleting it).
pub(crate) async fn unload(shared: &Shared, model_id: &str) {
    let mut proc = shared.llama.proc.lock().await;
    if proc.as_ref().is_some_and(|r| r.model_id == model_id) {
        if let Some(running) = proc.take() {
            running.stop().await;
        }
        shared.llama.set_status(shared, ProcStatus::Stopped);
    }
}

/// Clean shutdown.
pub(crate) async fn stop(shared: &Shared) {
    if let Some(running) = shared.llama.proc.lock().await.take() {
        running.stop().await;
    }
    shared.llama.set_status(shared, ProcStatus::Stopped);
}

/// Why a start failed, and on which variant (None: before one was chosen).
#[derive(Debug)]
struct StartError {
    variant: Option<String>,
    why: String,
}

/// The context a model runs with: its settings, else its own, else the default.
pub(crate) fn context_for(model: &Manifest, settings: &InferenceSettings) -> u32 {
    model
        .settings
        .context_length
        .or(model.context_length)
        .unwrap_or(settings.default_context)
        .max(MIN_CONTEXT)
}

/// Starts llama-server with `model` on the runtime variant to use, and
/// waits until it's ready.
async fn start(shared: &Arc<Shared>, model: &Manifest, settings: &InferenceSettings) -> Result<Running, StartError> {
    // Installs (or checks) a variant only if that hasn't happened yet.
    let s = shared.clone();
    let reserve = settings.disk_reserve_mb.saturating_mul(1024 * 1024);
    let resolved = tokio::task::spawn_blocking(move || {
        s.runtime.resolve(
            s.store.agent(),
            &std::sync::atomic::AtomicBool::new(false),
            reserve,
            &mut |_, _| {},
            &|| s.changed(),
        )
    })
    .await
    .map_err(|e| StartError {
        variant: None,
        why: e.to_string(),
    })?
    .map_err(|e| StartError {
        variant: None,
        why: format!("couldn't get llama.cpp: {e}"),
    })?;
    let variant = resolved.variant.clone();
    start_on(shared, model, settings, &resolved.exe, &variant)
        .await
        .map_err(|why| StartError {
            variant: Some(variant),
            why,
        })
}

async fn start_on(
    shared: &Arc<Shared>,
    model: &Manifest,
    settings: &InferenceSettings,
    exe: &std::path::Path,
    variant: &str,
) -> Result<Running, String> {
    let model_path = shared.store.model_path(&model.id);
    let context = u64::from(context_for(model, settings));
    let parallel = settings.max_concurrent.max(1);
    let port = free_port().map_err(|e| format!("couldn't find a free port: {e}"))?;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let api_key = hex::encode(rand::rng().random::<[u8; 32]>());
    let gpu = GpuArgs {
        variant: variant.to_string(),
        layers: model.settings.gpu_layers,
        fit_margin_mib: settings.gpu_fit_margin_mib,
    };
    let args = server_args(&model_path, port, context, parallel, settings.threads, &gpu);
    info!(
        target: "llama",
        "starting llama-server ({variant}) for {} on {addr} ({} tokens of context, {parallel} slot(s), GPU layers {:?})",
        model.id,
        context,
        model.settings.gpu_layers
    );
    debug!(target: "llama", "{} {}", exe.display(), args.join(" "));

    let mut cmd = Command::new(exe);
    cmd.args(&args)
        .env("LLAMA_API_KEY", &api_key)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    process::before_spawn(cmd.as_std_mut());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't start {}: {e}", exe.display()))?;
    process::after_spawn_tokio(&child);

    let tail = Tail::default();
    let offload = Arc::new(Mutex::new(OffloadLog::default()));
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(capture(BufReader::new(stdout), tail.clone(), offload.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(capture(BufReader::new(stderr), tail.clone(), offload.clone()));
    }

    let started = Instant::now();
    let timeout = Duration::from_millis(settings.model_load_timeout_ms);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            // Give the readers a moment to collect its last words.
            tokio::time::sleep(Duration::from_millis(100)).await;
            return Err(format!(
                "llama-server exited while loading ({status}). Its last output:\n{}",
                tail.report()
            ));
        }
        if let Ok(Ok(response)) = tokio::time::timeout(
            Duration::from_secs(2),
            local_request(addr, "GET", "/health", &api_key, None),
        )
        .await
        {
            if response.status == 200 {
                break;
            }
        }
        if started.elapsed() >= timeout {
            // Dropping `child` kills it.
            return Err(format!(
                "the model didn't load within {} s (INFERENCE_MODEL_LOAD_TIMEOUT_MS). llama-server's last output:\n{}",
                timeout.as_secs(),
                tail.report()
            ));
        }
        tokio::time::sleep(HEALTH_POLL).await;
    }
    // The log readers may be a line behind the health check.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let run = offload.lock().unwrap_or_else(|e| e.into_inner()).run_info(variant);
    info!(
        target: "llama",
        "{} loaded in {:.1} s on {variant}: {}",
        model.id,
        started.elapsed().as_secs_f32(),
        match (&run.gpu_layers, &run.device) {
            (Some(l), Some(d)) => format!("{}/{} layers on {d}", l.offloaded, l.total),
            (Some(l), None) => format!("{}/{} layers on the GPU", l.offloaded, l.total),
            _ => "CPU only".into(),
        }
    );
    Ok(Running {
        generation: shared.llama.next_generation.fetch_add(1, Ordering::Relaxed),
        model_id: model.id.clone(),
        run,
        child,
        addr,
        api_key,
        tail,
        users: Arc::new(AtomicUsize::new(0)),
        last_used: Arc::new(Mutex::new(Instant::now())),
    })
}

/// What reaches the GPU.
pub(super) struct GpuArgs {
    pub(super) variant: String,
    pub(super) layers: GpuLayers,
    pub(super) fit_margin_mib: u64,
}

/// Checked against `llama-server --help` of the pinned release (b12110:
/// `-ngl N|auto|all`, `--fit on|off`, `--fit-target MiB`), and again for
/// every new release by the runtime-smoke CI job (`smoke.rs`). The context
/// is per slot: llama-server splits `-c` across its `-np` slots. `--fit`
/// only adjusts what isn't set, so with `-c` given it chooses the GPU layers.
pub(super) fn server_args(
    model: &std::path::Path,
    port: u16,
    context: u64,
    parallel: u32,
    threads: u32,
    gpu: &GpuArgs,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-m".into(),
        model.to_string_lossy().into_owned(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "-c".into(),
        (context * u64::from(parallel)).to_string(),
        "-np".into(),
        parallel.to_string(),
        "--no-webui".into(),
        // Never fetch anything from the network.
        "--offline".into(),
    ];
    let layers = if gpu.variant == "cpu" {
        GpuLayers::Off
    } else {
        gpu.layers
    };
    match layers {
        GpuLayers::Off => args.extend(["-ngl".into(), "0".into(), "--fit".into(), "off".into()]),
        GpuLayers::All => args.extend(["-ngl".into(), "all".into(), "--fit".into(), "off".into()]),
        GpuLayers::Count(n) => args.extend(["-ngl".into(), n.to_string(), "--fit".into(), "off".into()]),
        GpuLayers::Auto => args.extend([
            "-ngl".into(),
            "auto".into(),
            "--fit".into(),
            "on".into(),
            "--fit-target".into(),
            gpu.fit_margin_mib.to_string(),
        ]),
    }
    if threads > 0 {
        args.extend(["-t".into(), threads.to_string()]);
    }
    args
}

async fn capture<R: tokio::io::AsyncRead + Unpin>(reader: BufReader<R>, tail: Tail, offload: Arc<Mutex<OffloadLog>>) {
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        debug!(target: "llama", "{line}");
        offload.lock().unwrap_or_else(|e| e.into_inner()).push(&line);
        tail.push(line);
    }
}

/// A port nothing is listening on right now.
fn free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn request_body(model_id: &str, req: &ChatCompletion, max_tokens: u32) -> Vec<u8> {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|t| json!({ "role": t.role, "content": t.content }))
        .collect();
    let mut body = json!({
        "model": model_id,
        "messages": messages,
        "stream": true,
        "max_tokens": max_tokens,
    });
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    serde_json::to_vec(&body).unwrap_or_default()
}

// ---------------- streaming ----------------

#[derive(Debug)]
pub(crate) enum StreamError {
    /// The connection failed or broke off.
    Io(std::io::Error),
    /// llama-server answered with an error.
    Backend(String),
    /// Nobody is listening for the reply any more.
    Closed,
}

/// POSTs a chat completion and forwards each `choices[0].delta.content`
/// as a `Delta`, until `[DONE]`. Returns the reply's token counts and
/// speed, if llama-server sent its timings.
pub(crate) async fn stream_chat(
    addr: SocketAddr,
    api_key: &str,
    body: &[u8],
    out: &mpsc::Sender<InferenceEvent>,
) -> Result<Option<Usage>, StreamError> {
    let mut response = local_request(addr, "POST", "/v1/chat/completions", api_key, Some(body))
        .await
        .map_err(StreamError::Io)?;
    if response.status != 200 {
        let text = response.body.read_to_end(64 * 1024).await.unwrap_or_default();
        return Err(StreamError::Backend(error_message(response.status, &text)));
    }
    let mut sse = SseParser::default();
    let mut finished = false;
    let mut usage = None;
    while let Some(bytes) = response.body.next().await.map_err(StreamError::Io)? {
        for data in sse.push(&bytes) {
            if data.trim() == "[DONE]" {
                return Ok(usage);
            }
            let chunk = parse_chunk(&data);
            if let Some(error) = chunk.error {
                return Err(StreamError::Backend(error));
            }
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
            if let Some(delta) = chunk.delta {
                out.send(InferenceEvent::Delta(delta))
                    .await
                    .map_err(|_| StreamError::Closed)?;
            }
            finished |= chunk.finished;
        }
    }
    if finished {
        Ok(usage)
    } else {
        Err(StreamError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the reply stream ended early",
        )))
    }
}

#[derive(Debug, Default, PartialEq)]
struct Chunk {
    delta: Option<String>,
    finished: bool,
    error: Option<String>,
    usage: Option<Usage>,
}

/// llama-server's `timings` (b12110, on the last chunk): `cache_n` and
/// `prompt_n` (prompt tokens reused and processed), `predicted_n`, and the
/// `prompt_per_second` and `predicted_per_second` speeds.
fn parse_timings(t: &Value) -> Option<Usage> {
    let count = |key: &str| t[key].as_u64().map(|n| u32::try_from(n).unwrap_or(u32::MAX));
    let speed = |key: &str| t[key].as_f64().filter(|v| v.is_finite() && *v >= 0.0);
    let completion = count("predicted_n")?;
    Some(Usage {
        prompt_tokens: count("prompt_n")
            .unwrap_or(0)
            .saturating_add(count("cache_n").unwrap_or(0)),
        completion_tokens: completion,
        tokens_per_second: speed("predicted_per_second"),
        prompt_tokens_per_second: speed("prompt_per_second"),
        run: None,
    })
}

fn parse_chunk(data: &str) -> Chunk {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return Chunk::default();
    };
    if let Some(error) = v.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| error.as_str().map(str::to_string))
            .unwrap_or_else(|| error.to_string());
        return Chunk {
            error: Some(message),
            ..Chunk::default()
        };
    }
    let choice = &v["choices"][0];
    Chunk {
        delta: choice["delta"]["content"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        finished: !choice["finish_reason"].is_null(),
        error: None,
        usage: v.get("timings").and_then(parse_timings),
    }
}

/// A readable message from an error response's body.
fn error_message(status: u16, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            let snippet: String = text.trim().chars().take(300).collect();
            if snippet.is_empty() {
                format!("llama-server answered HTTP {status}")
            } else {
                format!("llama-server answered HTTP {status}: {snippet}")
            }
        })
}

/// Server-sent events: collects `data:` lines and yields each event's data
/// when its blank line arrives. Bytes are buffered until a whole line is
/// in, so UTF-8 split across reads is fine.
#[derive(Default)]
struct SseParser {
    buf: Vec<u8>,
    data: Option<String>,
}

impl SseParser {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if let Some(data) = self.data.take() {
                    events.push(data);
                }
            } else if let Some(value) = line.strip_prefix("data:") {
                let value = value.strip_prefix(' ').unwrap_or(value);
                match &mut self.data {
                    Some(data) => {
                        data.push('\n');
                        data.push_str(value);
                    }
                    None => self.data = Some(value.to_string()),
                }
            }
        }
        events
    }
}

// ---------------- a minimal HTTP/1.1 client for 127.0.0.1 ----------------

pub(crate) struct LocalResponse {
    pub status: u16,
    pub body: Body,
}

pub(crate) struct Body {
    reader: BufReader<TcpStream>,
    kind: BodyKind,
}

enum BodyKind {
    Length(u64),
    Chunked { left: u64, done: bool },
    UntilClose,
}

const MAX_HEAD_LINE: usize = 16 * 1024;
const READ_CHUNK: usize = 16 * 1024;

fn bad(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string())
}

async fn read_line(reader: &mut BufReader<TcpStream>) -> std::io::Result<String> {
    let mut line = Vec::new();
    let n = (&mut *reader)
        .take(MAX_HEAD_LINE as u64)
        .read_until(b'\n', &mut line)
        .await?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection closed",
        ));
    }
    if !line.ends_with(b"\n") {
        return Err(bad("header line too long"));
    }
    Ok(String::from_utf8_lossy(&line).trim_end().to_string())
}

/// One request on a fresh connection (`Connection: close`).
pub(crate) async fn local_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    api_key: &str,
    body: Option<&[u8]>,
) -> std::io::Result<LocalResponse> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    let mut head =
        format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {api_key}\r\nConnection: close\r\n");
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\nAccept: text/event-stream\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    if let Some(body) = body {
        stream.write_all(body).await?;
    }
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let status_line = read_line(&mut reader).await?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| bad("malformed status line"))?;
    let mut length = None;
    let mut chunked = false;
    loop {
        let line = read_line(&mut reader).await?;
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
        if name == "content-length" {
            length = Some(value.parse::<u64>().map_err(|_| bad("bad content-length"))?);
        } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }
    let kind = match (chunked, length) {
        (true, _) => BodyKind::Chunked { left: 0, done: false },
        (false, Some(n)) => BodyKind::Length(n),
        (false, None) => BodyKind::UntilClose,
    };
    Ok(LocalResponse {
        status,
        body: Body { reader, kind },
    })
}

impl Body {
    /// The next piece of the decoded body; None at its end.
    pub async fn next(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; READ_CHUNK];
        match &mut self.kind {
            BodyKind::Length(0) => Ok(None),
            BodyKind::Length(left) => {
                let want = (*left).min(READ_CHUNK as u64) as usize;
                let n = self.reader.read(&mut buf[..want]).await?;
                if n == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "body cut short"));
                }
                *left -= n as u64;
                buf.truncate(n);
                Ok(Some(buf))
            }
            BodyKind::UntilClose => {
                let n = self.reader.read(&mut buf).await?;
                buf.truncate(n);
                Ok((n > 0).then_some(buf))
            }
            BodyKind::Chunked { left, done } => {
                if *done {
                    return Ok(None);
                }
                if *left == 0 {
                    let line = read_line(&mut self.reader).await?;
                    let size = line.split(';').next().unwrap_or("").trim();
                    let size = u64::from_str_radix(size, 16).map_err(|_| bad("bad chunk size"))?;
                    if size == 0 {
                        *done = true;
                        // Trailers, up to the blank line.
                        while !read_line(&mut self.reader).await?.is_empty() {}
                        return Ok(None);
                    }
                    *left = size;
                }
                let want = (*left).min(READ_CHUNK as u64) as usize;
                let n = self.reader.read(&mut buf[..want]).await?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "chunk cut short",
                    ));
                }
                *left -= n as u64;
                if *left == 0 {
                    let mut crlf = [0u8; 2];
                    self.reader.read_exact(&mut crlf).await?;
                }
                buf.truncate(n);
                Ok(Some(buf))
            }
        }
    }

    pub async fn read_to_end(&mut self, limit: usize) -> std::io::Result<Vec<u8>> {
        let mut all = Vec::new();
        while let Some(piece) = self.next().await? {
            all.extend_from_slice(&piece);
            if all.len() >= limit {
                break;
            }
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{ChatRole, ChatTurn};
    use tokio::net::TcpListener;

    #[test]
    fn sse_events_are_split_on_blank_lines_whatever_the_reads() {
        let stream = "data: {\"a\":1}\n\n: keep-alive\n\ndata: x\r\ndata: y\r\n\r\ndata: [DONE]\n\n";
        for split in [1, 3, 7, stream.len()] {
            let mut parser = SseParser::default();
            let mut events = Vec::new();
            for piece in stream.as_bytes().chunks(split) {
                events.extend(parser.push(piece));
            }
            assert_eq!(events, vec!["{\"a\":1}", "x\ny", "[DONE]"], "split {split}");
        }
        // A multi-byte character split across reads.
        let mut parser = SseParser::default();
        let bytes = "data: é\n\n".as_bytes();
        assert!(parser.push(&bytes[..7]).is_empty());
        assert_eq!(parser.push(&bytes[7..]), vec!["é"]);
    }

    #[test]
    fn chunks_and_errors_parse() {
        assert_eq!(
            parse_chunk(r#"{"choices":[{"delta":{"content":"Hi"},"finish_reason":null}]}"#),
            Chunk {
                delta: Some("Hi".into()),
                ..Chunk::default()
            }
        );
        assert!(parse_chunk(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#).finished);
        assert_eq!(
            parse_chunk(r#"{"error":{"code":500,"message":"boom"}}"#)
                .error
                .as_deref(),
            Some("boom")
        );
        assert_eq!(
            error_message(400, br#"{"error":{"message":"the request exceeds the context size"}}"#),
            "the request exceeds the context size"
        );
        assert_eq!(error_message(502, b""), "llama-server answered HTTP 502");
    }

    fn gpu(variant: &str, layers: GpuLayers) -> GpuArgs {
        GpuArgs {
            variant: variant.into(),
            layers,
            fit_margin_mib: 1024,
        }
    }

    #[test]
    fn args_match_the_pinned_llama_server() {
        let m = std::path::Path::new("/m/model.gguf");
        let base = "-m /m/model.gguf --host 127.0.0.1 --port 4242 -c 8192 -np 2 --no-webui --offline";
        let args = |g: GpuArgs| server_args(m, 4242, 4096, 2, 0, &g).join(" ");
        assert_eq!(
            args(gpu("cuda-13", GpuLayers::Auto)),
            format!("{base} -ngl auto --fit on --fit-target 1024")
        );
        assert_eq!(args(gpu("cpu", GpuLayers::Auto)), format!("{base} -ngl 0 --fit off"));
        assert_eq!(args(gpu("vulkan", GpuLayers::Off)), format!("{base} -ngl 0 --fit off"));
        assert_eq!(
            args(gpu("vulkan", GpuLayers::All)),
            format!("{base} -ngl all --fit off")
        );
        assert_eq!(
            args(gpu("cuda-12", GpuLayers::Count(12))),
            format!("{base} -ngl 12 --fit off")
        );
        assert!(
            !args(gpu("cpu", GpuLayers::Off)).contains("api-key"),
            "the key goes in the environment"
        );
        assert!(
            server_args(std::path::Path::new("m"), 1, 4096, 1, 6, &gpu("cpu", GpuLayers::Auto))
                .ends_with(&["-t".into(), "6".into()])
        );
    }

    /// A CUDA load as llama-server b12110 logs it. Built from the format
    /// strings in b12110's `src/llama.cpp` and `src/llama-model.cpp`
    /// (`using device …`, `offloaded %d/%d layers to GPU`); replace with a
    /// captured log once one is recorded on NVIDIA hardware.
    const CUDA_LOAD_LOG: &str = "\
ggml_cuda_init: found 1 CUDA devices (Total VRAM: 8191 MiB):
  Device 0: NVIDIA GeForce RTX 3070, compute capability 8.6, VMM: yes, VRAM: 8191 MiB
load_backend: loaded CUDA backend from /data/runtime/cuda-13-b12110/libggml-cuda.so
common_fit_params: fitting params to device memory, for bugs during this step try to reproduce them with -fit off
llama_model_load_from_file_impl: using device CUDA0 (NVIDIA GeForce RTX 3070) (0000:01:00.0) - 7347 MiB free
llama_model_loader: loaded meta data with 26 key-value pairs and 291 tensors from model.gguf (version GGUF V3 (latest))
print_info: n_layer          = 28
load_tensors: loading model tensors, this can take a while... (mmap = true, direct_io = false)
load_tensors: offloading output layer to GPU
load_tensors: offloading 28 repeating layers to GPU
load_tensors: offloaded 29/29 layers to GPU
load_tensors:        CUDA0 model buffer size =  4168.09 MiB
load_tensors:   CPU_Mapped model buffer size =   292.36 MiB
srv    load_model: initializing slots, n_slots = 1
main: model loaded
";

    #[test]
    fn the_offload_summary_is_read_from_the_log() {
        let mut log = OffloadLog::default();
        for line in CUDA_LOAD_LOG.lines() {
            log.push(line);
        }
        assert_eq!(
            log.run_info("cuda-13"),
            RunInfo {
                variant: "cuda-13".into(),
                gpu_layers: Some(GpuOffload {
                    offloaded: 29,
                    total: 29
                }),
                device: Some("NVIDIA GeForce RTX 3070".into()),
            }
        );
        // Off: nothing on the GPU, so no device.
        let mut off = OffloadLog::default();
        off.push("llama_model_load_from_file_impl: using device Vulkan0 (AMD Radeon RX 7900 XTX (RADV NAVI31)) (0000:03:00.0) - 24000 MiB free");
        off.push("load_tensors: offloaded 0/29 layers to GPU");
        assert_eq!(off.run_info("vulkan").device, None);
        assert_eq!(off.devices, ["AMD Radeon RX 7900 XTX (RADV NAVI31)"]);
        // The CPU build never prints a summary.
        let cpu = OffloadLog::default().run_info("cpu");
        assert_eq!((cpu.gpu_layers, cpu.device), (None, None));
        assert_eq!(parse_offloaded("offloaded x/29 layers to GPU"), None);
        assert_eq!(parse_using_device("using device"), None);
    }

    #[test]
    fn timings_become_usage() {
        // The final streamed chunk of b12110's /v1/chat/completions.
        let last = r#"{"choices":[{"finish_reason":"stop","index":0,"delta":{}}],"created":1,"id":"chatcmpl-1","model":"m","system_fingerprint":"b12110-0","object":"chat.completion.chunk","timings":{"cache_n":3,"prompt_n":20,"prompt_ms":40.1,"prompt_per_token_ms":2.0,"prompt_per_second":498.7,"predicted_n":64,"predicted_ms":1250.0,"predicted_per_token_ms":19.5,"predicted_per_second":51.2}}"#;
        let chunk = parse_chunk(last);
        assert!(chunk.finished);
        assert_eq!(
            chunk.usage,
            Some(Usage {
                prompt_tokens: 23,
                completion_tokens: 64,
                tokens_per_second: Some(51.2),
                prompt_tokens_per_second: Some(498.7),
                run: None,
            })
        );
        assert_eq!(parse_chunk(r#"{"choices":[{"delta":{"content":"x"}}]}"#).usage, None);
    }

    #[test]
    fn slots_cap_concurrent_requests() {
        let active = Arc::new(AtomicUsize::new(0));
        let a = Slot::take(&active, 2).unwrap();
        let _b = Slot::take(&active, 2).unwrap();
        assert!(Slot::take(&active, 2).is_none());
        drop(a);
        assert!(Slot::take(&active, 2).is_some());
    }

    /// What the fake llama-server does with one request.
    enum Script {
        /// SSE events (each a `data:` payload), chunked, then close.
        Events(Vec<String>),
        /// A plain error response.
        Status(u16, String),
        /// One event, then wait for the client to hang up; reports how
        /// long that took on the channel.
        OneThenWait(tokio::sync::oneshot::Sender<Duration>),
    }

    /// A one-request stand-in for llama-server. Returns its address and the
    /// request it received.
    async fn fake_server(script: Script) -> (SocketAddr, tokio::sync::oneshot::Receiver<(String, Value)>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (req_tx, req_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                head.push_str(&line);
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).await.unwrap();
            let _ = req_tx.send((head, serde_json::from_slice(&body).unwrap_or(Value::Null)));
            let mut stream = reader.into_inner();
            let chunk = |s: &str| format!("{:x}\r\n{s}\r\n", s.len());
            match script {
                Script::Events(events) => {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    for e in events {
                        stream
                            .write_all(chunk(&format!("data: {e}\n\n")).as_bytes())
                            .await
                            .unwrap();
                    }
                    stream.write_all(b"0\r\n\r\n").await.unwrap();
                }
                Script::Status(code, body) => {
                    let response = format!("HTTP/1.1 {code} Error\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
                Script::OneThenWait(done) => {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                        .await
                        .unwrap();
                    let first = r#"data: {"choices":[{"delta":{"content":"first"}}]}"#;
                    stream
                        .write_all(chunk(&format!("{first}\n\n")).as_bytes())
                        .await
                        .unwrap();
                    let started = Instant::now();
                    let mut buf = [0u8; 64];
                    // Returns 0 once the client has closed its end.
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => continue,
                        }
                    }
                    let _ = done.send(started.elapsed());
                }
            }
        });
        (addr, req_rx)
    }

    fn delta(text: &str) -> String {
        json!({ "choices": [{ "index": 0, "delta": { "content": text }, "finish_reason": null }] }).to_string()
    }

    fn completion() -> ChatCompletion {
        ChatCompletion {
            model_id: "m".into(),
            messages: vec![
                ChatTurn {
                    role: ChatRole::User,
                    content: "hi".into(),
                },
                ChatTurn {
                    role: ChatRole::Assistant,
                    content: "hello".into(),
                },
                ChatTurn {
                    role: ChatRole::User,
                    content: "again".into(),
                },
            ],
            max_tokens: None,
            temperature: None,
        }
    }

    async fn collect(rx: &mut mpsc::Receiver<InferenceEvent>) -> Vec<String> {
        let mut deltas = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let InferenceEvent::Delta(d) = event {
                deltas.push(d);
            }
        }
        deltas
    }

    #[tokio::test]
    async fn deltas_arrive_in_order_and_the_request_is_openai_shaped() {
        let events = vec![
            json!({ "choices": [{ "delta": { "role": "assistant" } }] }).to_string(),
            delta("Hel"),
            delta("lo"),
            delta(" world"),
            json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }).to_string(),
            "[DONE]".into(),
        ];
        let (addr, request) = fake_server(Script::Events(events)).await;
        let (tx, mut rx) = mpsc::channel(64);
        let body = request_body("m", &completion(), 2048);
        assert_eq!(
            stream_chat(addr, "secret", &body, &tx).await.unwrap(),
            None,
            "no timings sent"
        );
        assert_eq!(collect(&mut rx).await, vec!["Hel", "lo", " world"]);

        let (head, body) = request.await.unwrap();
        assert!(head.starts_with("POST /v1/chat/completions HTTP/1.1"), "{head}");
        assert!(head.contains("Authorization: Bearer secret"), "{head}");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 2048);
        assert!(body.get("temperature").is_none(), "only sent when the request has one");
        assert_eq!(body["messages"][2], json!({ "role": "user", "content": "again" }));

        let mut with_t = completion();
        with_t.temperature = Some(0.25);
        let body: Value = serde_json::from_slice(&request_body("m", &with_t, 10)).unwrap();
        assert_eq!(body["temperature"], 0.25);
    }

    #[tokio::test]
    async fn errors_become_backend_errors() {
        let (addr, _) = fake_server(Script::Status(
            400,
            r#"{"error":{"code":400,"message":"the request exceeds the available context size"}}"#.into(),
        ))
        .await;
        let (tx, _rx) = mpsc::channel(64);
        let r = stream_chat(addr, "k", b"{}", &tx).await;
        assert!(
            matches!(&r, Err(StreamError::Backend(m)) if m.contains("context size")),
            "{r:?}"
        );

        let (addr, _) = fake_server(Script::Events(vec![
            delta("par"),
            json!({ "error": { "message": "out of memory" } }).to_string(),
        ]))
        .await;
        let (tx, mut rx) = mpsc::channel(64);
        let r = stream_chat(addr, "k", b"{}", &tx).await;
        assert!(
            matches!(&r, Err(StreamError::Backend(m)) if m == "out of memory"),
            "{r:?}"
        );
        assert_eq!(collect(&mut rx).await, vec!["par"]);

        // Ending without [DONE] or a finish_reason is a broken stream.
        let (addr, _) = fake_server(Script::Events(vec![delta("cut")])).await;
        let (tx, _rx) = mpsc::channel(64);
        assert!(matches!(
            stream_chat(addr, "k", b"{}", &tx).await,
            Err(StreamError::Io(_))
        ));

        // Nothing listening.
        let port = free_port().unwrap();
        let (tx, _rx) = mpsc::channel(64);
        assert!(matches!(
            stream_chat(SocketAddr::from(([127, 0, 0, 1], port)), "k", b"{}", &tx).await,
            Err(StreamError::Io(_))
        ));
    }

    #[tokio::test]
    async fn dropping_the_future_closes_the_connection() {
        let (hung_up_tx, hung_up) = tokio::sync::oneshot::channel();
        let (addr, _) = fake_server(Script::OneThenWait(hung_up_tx)).await;
        let (tx, mut rx) = mpsc::channel(64);
        let task = tokio::spawn(async move {
            let _ = stream_chat(addr, "k", b"{}", &tx).await;
        });
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(InferenceEvent::Delta(d))) => assert_eq!(d, "first"),
            other => panic!("expected the first delta, got {other:?}"),
        }
        // What the session does on chat_cancel.
        task.abort();
        let waited = tokio::time::timeout(Duration::from_secs(5), hung_up)
            .await
            .expect("the server never saw the connection close")
            .unwrap();
        assert!(waited < Duration::from_secs(2), "{waited:?}");
    }

    #[test]
    fn the_tail_keeps_the_last_lines() {
        let tail = Tail::default();
        assert_eq!(tail.report(), "(it printed nothing)");
        for i in 0..80 {
            tail.push(format!("line {i}"));
        }
        let report = tail.report();
        assert!(
            report.starts_with("line 30\n") && report.ends_with("line 79"),
            "{report}"
        );
        tail.push("x".repeat(10_000));
        assert!(tail.report().chars().count() <= TAIL_MAX_CHARS + 1);
    }
}
