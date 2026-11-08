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
//!   kill-on-close job object on Windows, and a kill on drop and on clean
//!   shutdown everywhere.
//! - A reply streams over its own local HTTP connection. Dropping the
//!   `chat` future drops the socket, and llama-server stops generating
//!   when its client goes away. That's why this talks HTTP/1.1 by hand over
//!   a tokio socket instead of using the (blocking) ureq: a blocked read
//!   would only notice the cancel after the next token.

use super::store::Manifest;
use super::{ChatCompletion, InferenceError, InferenceEvent, Shared};
use log::{debug, info, warn};
use protocol::models::MIN_CONTEXT;
use protocol::{InferenceSettings, Stage};
use rand::Rng;
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
}

impl Default for LlamaCpp {
    fn default() -> Self {
        LlamaCpp {
            proc: tokio::sync::Mutex::new(None),
            status: Mutex::new(ProcStatus::Stopped),
            active: Arc::new(AtomicUsize::new(0)),
            next_generation: AtomicU64::new(1),
        }
    }
}

impl LlamaCpp {
    pub fn status(&self) -> ProcStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_status(&self, shared: &Shared, status: ProcStatus) {
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

struct Running {
    generation: u64,
    model_id: String,
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
) -> Result<(), InferenceError> {
    let settings = shared.settings();
    let _slot = Slot::take(&shared.llama.active, settings.max_concurrent).ok_or(InferenceError::Busy)?;
    let lease = acquire(shared, model, &settings, out).await?;
    out.send(InferenceEvent::Status(Stage::Generating))
        .await
        .map_err(|_| InferenceError::Closed)?;
    let body = request_body(&model.id, req, max_tokens);
    match stream_chat(lease.addr, &lease.api_key, &body, out).await {
        Ok(()) => Ok(()),
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
    let running = start(shared, model, settings)
        .await
        .map_err(InferenceError::LoadFailed)?;
    guard.armed = false;
    let lease = running.lease();
    let generation = running.generation;
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

/// Starts llama-server with `model` and waits until it's ready.
async fn start(shared: &Arc<Shared>, model: &Manifest, settings: &InferenceSettings) -> Result<Running, String> {
    let exe = match shared.runtime.server_exe() {
        Some(exe) => exe,
        // Only if the runtime was removed after the model was pulled.
        None => {
            let s = shared.clone();
            tokio::task::spawn_blocking(move || {
                s.runtime.ensure(
                    s.store.agent(),
                    &std::sync::atomic::AtomicBool::new(false),
                    &mut |_, _| {},
                    &|| s.changed(),
                )
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("couldn't get llama.cpp: {e}"))?
        }
    };
    let model_path = shared.store.model_path(&model.id);
    let context = u64::from(
        model
            .context_length
            .unwrap_or(settings.default_context)
            .max(MIN_CONTEXT),
    );
    let parallel = settings.max_concurrent.max(1);
    let port = free_port().map_err(|e| format!("couldn't find a free port: {e}"))?;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let api_key = hex::encode(rand::thread_rng().gen::<[u8; 32]>());
    let args = server_args(&model_path, port, context, parallel, settings.threads);
    info!(
        target: "llama",
        "starting llama-server for {} on {addr} ({} tokens of context, {parallel} slot(s))",
        model.id,
        context
    );
    debug!(target: "llama", "{} {}", exe.display(), args.join(" "));

    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .env("LLAMA_API_KEY", &api_key)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    platform::before_spawn(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't start {}: {e}", exe.display()))?;
    platform::after_spawn(&child);

    let tail = Tail::default();
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(capture(BufReader::new(stdout), tail.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(capture(BufReader::new(stderr), tail.clone()));
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
    info!(target: "llama", "{} loaded in {:.1} s", model.id, started.elapsed().as_secs_f32());
    Ok(Running {
        generation: shared.llama.next_generation.fetch_add(1, Ordering::Relaxed),
        model_id: model.id.clone(),
        child,
        addr,
        api_key,
        tail,
        users: Arc::new(AtomicUsize::new(0)),
        last_used: Arc::new(Mutex::new(Instant::now())),
    })
}

/// Checked against `llama-server --help` of the pinned release. The context
/// is per slot: llama-server splits `-c` across its `-np` slots.
fn server_args(model: &std::path::Path, port: u16, context: u64, parallel: u32, threads: u32) -> Vec<String> {
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
        // CPU only until GPU builds are pinned.
        "-ngl".into(),
        "0".into(),
        "--no-webui".into(),
        // Never fetch anything from the network.
        "--offline".into(),
    ];
    if threads > 0 {
        args.extend(["-t".into(), threads.to_string()]);
    }
    args
}

async fn capture<R: tokio::io::AsyncRead + Unpin>(reader: BufReader<R>, tail: Tail) {
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        debug!(target: "llama", "{line}");
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
/// as a `Delta`, until `[DONE]`.
pub(crate) async fn stream_chat(
    addr: SocketAddr,
    api_key: &str,
    body: &[u8],
    out: &mpsc::Sender<InferenceEvent>,
) -> Result<(), StreamError> {
    let mut response = local_request(addr, "POST", "/v1/chat/completions", api_key, Some(body))
        .await
        .map_err(StreamError::Io)?;
    if response.status != 200 {
        let text = response.body.read_to_end(64 * 1024).await.unwrap_or_default();
        return Err(StreamError::Backend(error_message(response.status, &text)));
    }
    let mut sse = SseParser::default();
    let mut finished = false;
    while let Some(bytes) = response.body.next().await.map_err(StreamError::Io)? {
        for data in sse.push(&bytes) {
            if data.trim() == "[DONE]" {
                return Ok(());
            }
            let chunk = parse_chunk(&data);
            if let Some(error) = chunk.error {
                return Err(StreamError::Backend(error));
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
        Ok(())
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

// ---------------- the process must never outlive the client ----------------

#[cfg(target_os = "linux")]
mod platform {
    use tokio::process::{Child, Command};

    /// Linux kills the child when the thread that spawned it exits. That's
    /// a tokio worker thread here (never `spawn_blocking`, whose threads
    /// come and go), and those live as long as the runtime, i.e. the client.
    pub fn before_spawn(cmd: &mut Command) {
        // SAFETY: getpid is async-signal-safe and runs before the fork.
        let parent = unsafe { libc::getpid() };
        // SAFETY: the closure runs in the forked child before exec and only
        // calls async-signal-safe functions (prctl, getppid, _exit).
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent died before prctl took effect.
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }

    pub fn after_spawn(_child: &Child) {}
}

#[cfg(windows)]
mod platform {
    use log::warn;
    use std::sync::OnceLock;
    use tokio::process::{Child, Command};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// No console window flashing up from the tray app.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub fn before_spawn(cmd: &mut Command) {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    /// One job object for the life of the client, never closed: Windows
    /// closes it when the client exits, however it exits, and that kills
    /// every process in it.
    fn job() -> Option<HANDLE> {
        static JOB: OnceLock<Option<usize>> = OnceLock::new();
        let raw = JOB.get_or_init(|| {
            // SAFETY: plain Win32 calls; `info` outlives the call that reads it.
            unsafe {
                let job = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?;
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .ok()?;
                Some(job.0 as usize)
            }
        });
        raw.map(|h| HANDLE(h as *mut core::ffi::c_void))
    }

    pub fn after_spawn(child: &Child) {
        let (Some(job), Some(process)) = (job(), child.raw_handle()) else {
            warn!(target: "llama", "couldn't put llama-server in a job object; it may outlive a crashed client");
            return;
        };
        // SAFETY: both handles are valid; the child's stays open while `child` lives.
        if let Err(e) = unsafe { AssignProcessToJobObject(job, HANDLE(process)) } {
            warn!(target: "llama", "couldn't put llama-server in a job object: {e}");
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use tokio::process::{Child, Command};
    pub fn before_spawn(_cmd: &mut Command) {}
    pub fn after_spawn(_child: &Child) {}
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

    #[test]
    fn args_match_the_pinned_llama_server() {
        let args = server_args(std::path::Path::new("/m/model.gguf"), 4242, 4096, 2, 0);
        let joined = args.join(" ");
        assert_eq!(
            joined,
            "-m /m/model.gguf --host 127.0.0.1 --port 4242 -c 8192 -np 2 -ngl 0 --no-webui --offline"
        );
        assert!(!joined.contains("api-key"), "the key goes in the environment");
        assert!(server_args(std::path::Path::new("m"), 1, 4096, 1, 6).ends_with(&["-t".into(), "6".into()]));
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
        stream_chat(addr, "secret", &body, &tx).await.unwrap();
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
