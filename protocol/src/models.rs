//! Models on clients: what a client has (`model_inventory`), the commands
//! the server sends to change that (`model_command`), and how they went
//! (`model_command_status`).
//!
//! The server decides which models a client should have; the client only
//! executes commands and reports. The command set is meant to grow (load
//! and unload with per-model settings, choosing among pinned runtime
//! variants), so a kind this build doesn't know parses as
//! [`ModelCommandKind::Unknown`] and is answered `unsupported` rather than
//! breaking the connection.
//!
//! Model ids come from the server and name directories on the client, so
//! both sides check them with [`valid_model_id`].

use serde::{Deserialize, Serialize};

/// The longest model id either side accepts.
pub const MODEL_ID_MAX_LEN: usize = 100;

/// Lowercase ASCII letters, digits, `.`, `_` and `-`, 1 to
/// [`MODEL_ID_MAX_LEN`] characters, and not made only of dots (so never
/// `.` or `..`). The client builds a directory name from it and from
/// nothing else the server sends.
pub fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MODEL_ID_MAX_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
        && !id.bytes().all(|b| b == b'.')
}

/// What runs a model.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// A GGUF file run by a managed `llama-server`.
    LlamaCpp,
    /// The built-in test model: replies with the user's last message.
    Echo,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    /// Being pulled.
    Downloading,
    /// On disk and verified; loads on the first request.
    Available,
    /// Being loaded into memory.
    Loading,
    /// In memory, ready to answer.
    Loaded,
    /// The last pull failed; `LocalModel::error` says why.
    Failed,
}

/// Descriptive details the suggestion engine and the dashboard use. All
/// optional: a model without them still works.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ModelMeta {
    /// e.g. "qwen2.5".
    pub family: Option<String>,
    /// e.g. "0.5B".
    pub parameters: Option<String>,
    /// e.g. "Q4_K_M".
    pub quantization: Option<String>,
}

/// One model a client has (or is getting).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LocalModel {
    pub id: String,
    pub display_name: String,
    pub backend: Backend,
    pub state: ModelState,
    pub size_bytes: u64,
    /// The context it runs with, in tokens. None: the client's default.
    #[serde(default)]
    pub context_length: Option<u32>,
    #[serde(default)]
    pub meta: ModelMeta,
    /// Why it's `failed`.
    #[serde(default)]
    pub error: Option<String>,
    /// Its settings from the dashboard (`configure`).
    #[serde(default)]
    pub settings: ModelSettings,
    /// What it's running on, while `loaded`.
    #[serde(default)]
    pub run: Option<RunInfo>,
}

/// How many of a model's layers go to the GPU. On the wire: `"auto"`,
/// `"off"`, `"all"` or a number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GpuLayers {
    /// llama.cpp fits as many as the free VRAM allows (`--fit`).
    #[default]
    Auto,
    /// CPU only.
    Off,
    All,
    Count(u32),
}

impl Serialize for GpuLayers {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            GpuLayers::Auto => s.serialize_str("auto"),
            GpuLayers::Off => s.serialize_str("off"),
            GpuLayers::All => s.serialize_str("all"),
            GpuLayers::Count(n) => s.serialize_u32(*n),
        }
    }
}

impl<'de> Deserialize<'de> for GpuLayers {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Word(String),
            Count(u32),
        }
        match Repr::deserialize(d)? {
            Repr::Count(n) => Ok(GpuLayers::Count(n)),
            Repr::Word(w) => match w.as_str() {
                "auto" => Ok(GpuLayers::Auto),
                "off" => Ok(GpuLayers::Off),
                "all" => Ok(GpuLayers::All),
                other => Err(serde::de::Error::custom(format!(
                    "gpu_layers must be \"auto\", \"off\", \"all\" or a number, got {other:?}"
                ))),
            },
        }
    }
}

/// The most layers a `Count` may ask for; no model has nearly this many.
pub const MAX_GPU_LAYERS: u32 = 10_000;
/// The largest context `configure` may set.
pub const MAX_CONTEXT: u32 = 1 << 22;

/// Per-model settings, stored in the model's manifest on the client and
/// applied the next time it loads.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ModelSettings {
    pub gpu_layers: GpuLayers,
    /// None: the model's own (or the client's default).
    pub context_length: Option<u32>,
}

impl ModelSettings {
    /// What's wrong with these settings, if anything.
    pub fn problem(&self) -> Option<String> {
        if let GpuLayers::Count(n) = self.gpu_layers {
            if n > MAX_GPU_LAYERS {
                return Some(format!("gpu_layers can be at most {MAX_GPU_LAYERS}"));
            }
        }
        match self.context_length {
            Some(c) if !(MIN_CONTEXT..=MAX_CONTEXT).contains(&c) => Some(format!(
                "context_length must be between {MIN_CONTEXT} and {MAX_CONTEXT}"
            )),
            _ => None,
        }
    }
}

/// Layers on the GPU, from llama-server's log ("offloaded 29/29 layers to GPU").
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuOffload {
    pub offloaded: u32,
    pub total: u32,
}

/// Where a loaded model runs: the runtime variant and what reached the GPU.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct RunInfo {
    /// e.g. "cuda-13".
    pub variant: String,
    /// None for a build without GPU support (it doesn't say).
    pub gpu_layers: Option<GpuOffload>,
    /// The GPU(s) it uses, e.g. "NVIDIA GeForce RTX 3070". None: CPU only.
    pub device: Option<String>,
}

/// Token counts and speed of one reply, from llama-server's timings.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Generation speed.
    pub tokens_per_second: Option<f64>,
    /// Prompt processing speed.
    pub prompt_tokens_per_second: Option<f64>,
    /// What generated it.
    pub run: Option<RunInfo>,
}

/// A llama.cpp release: `b` plus a build number, e.g. "b12110". Compares by
/// build number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LlamaCppVersion(pub u32);

impl LlamaCppVersion {
    pub fn parse(s: &str) -> Option<LlamaCppVersion> {
        let digits = s.strip_prefix('b')?;
        if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok().map(LlamaCppVersion)
    }
}

impl std::fmt::Display for LlamaCppVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "b{}", self.0)
    }
}

/// The data volume models live on.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Storage {
    /// Everything under the models directory, partial downloads included.
    pub models_bytes: u64,
    /// Free space on the volume. 0 if it couldn't be read.
    pub free_bytes: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    /// Not downloaded yet; it will be on the first pull or load.
    Absent,
    Downloading,
    Ready,
    /// It didn't work here (no GPU it can use, or it crashed);
    /// `RuntimeInfo::error` says why. Not tried again until the client
    /// restarts or the server selects it.
    Failed,
}

/// A runtime (inference engine build) this client has pinned. Only pinned
/// builds exist: executables never come from the server, which can only
/// choose among them (`select_runtime`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub backend: Backend,
    /// The pinned release, e.g. "b12110".
    pub version: String,
    /// "cpu", "cuda-12", "cuda-13" or "vulkan".
    pub variant: String,
    pub state: RuntimeState,
    /// The one models load with.
    #[serde(default)]
    pub selected: bool,
    /// Chosen automatically from the hardware, not by the server.
    #[serde(default)]
    pub automatic: bool,
    /// The GPUs it found (`--list-devices`), once it's been checked.
    #[serde(default)]
    pub devices: Vec<String>,
    /// Why it `failed`.
    #[serde(default)]
    pub error: Option<String>,
}

/// Everything about models on one client: `model_inventory`. Sent after
/// `welcome`, and again whenever it changes (debounced). Replaces the
/// previous one.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub models: Vec<LocalModel>,
    pub storage: Storage,
    pub runtimes: Vec<RuntimeInfo>,
}

/// Where a model file comes from, and what it must be.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ModelSource {
    /// HTTPS only.
    pub url: String,
    /// Lowercase hex SHA-256 of the whole file.
    pub sha256: String,
    pub size_bytes: u64,
}

/// A model the server wants on a client.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: String,
    pub display_name: String,
    pub backend: Backend,
    pub source: ModelSource,
    #[serde(default)]
    pub context_length: Option<u32>,
    #[serde(default)]
    pub meta: ModelMeta,
    /// The oldest llama.cpp release that can run it (e.g. "b12110"). A
    /// client with an older one refuses the pull.
    #[serde(default)]
    pub min_runtime_version: Option<String>,
}

/// What a `model_command` asks for.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelCommandKind {
    /// Download and verify a model (and the runtime it needs, if that isn't
    /// there yet). Already present and verified: done at once.
    Pull { model: ModelSpec },
    /// Unload it if it's loaded, then remove it. Missing: done at once.
    Delete { model_id: String },
    /// Stop another command (a pull). A cancelled pull keeps its partial
    /// file, so pulling again resumes.
    Cancel { target_command_id: String },
    /// Use this llama.cpp variant from now on ("auto" restores the
    /// automatic choice). Must be a variant this client has pinned;
    /// anything else fails. Unloads the current model if the variant
    /// changes, and installs the variant if it isn't yet.
    SelectRuntime { variant: String },
    /// Settings for one installed model, stored in its manifest and applied
    /// the next time it loads (unloading it now if it's loaded).
    Configure { model_id: String, settings: ModelSettings },
    /// Any kind this client doesn't know (a newer server). Answered
    /// `unsupported`.
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandState {
    Accepted,
    InProgress,
    Done,
    Failed,
    Cancelled,
    /// The client doesn't know this kind of command.
    Unsupported,
}

impl CommandState {
    /// No more statuses follow.
    pub fn is_final(self) -> bool {
        !matches!(self, CommandState::Accepted | CommandState::InProgress)
    }
}

/// Which download a pull's progress is about: the runtime it needed first,
/// or the model itself.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPhase {
    Runtime,
    Model,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub phase: ProgressPhase,
}

/// Where a reply is before its first text: `chat_progress`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// The model is being loaded into memory (can take a while).
    LoadingModel,
    /// The model is processing the prompt or producing text.
    Generating,
}

/// The inference values from the server's `config.env`, pushed in
/// `welcome` as part of `ClientSettings`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferenceSettings {
    /// Pull progress is reported at most this often.
    pub progress_interval_ms: u64,
    /// Stop `llama-server` after this long with no requests. 0 never does.
    pub idle_unload_secs: u64,
    /// How long a model may take to load.
    pub model_load_timeout_ms: u64,
    /// Requests generated at once; more fail with "client busy".
    pub max_concurrent: u32,
    /// The longest reply, in tokens.
    pub max_tokens: u32,
    /// Context size for a model that doesn't specify one.
    pub default_context: u32,
    /// CPU threads for llama.cpp; 0 lets it decide.
    pub threads: u32,
    /// Free space a pull must leave on the data volume.
    pub disk_reserve_mb: u64,
    /// Offer the built-in `echo` test model. Off unless the server turns it on.
    pub echo_model_enabled: bool,
    /// VRAM llama.cpp leaves free when it fits a model on the GPU.
    #[serde(default = "default_gpu_fit_margin_mib")]
    pub gpu_fit_margin_mib: u64,
}

fn default_gpu_fit_margin_mib() -> u64 {
    1_024
}

/// Progress reports can't come faster than this.
pub const MIN_PROGRESS_INTERVAL_MS: u64 = 100;
/// A model gets at least this long to load.
pub const MIN_MODEL_LOAD_TIMEOUT_MS: u64 = 1_000;
/// The smallest context the client will run a model with.
pub const MIN_CONTEXT: u32 = 256;

impl Default for InferenceSettings {
    fn default() -> Self {
        InferenceSettings {
            progress_interval_ms: 1_000,
            idle_unload_secs: 600,
            model_load_timeout_ms: 120_000,
            max_concurrent: 1,
            max_tokens: 2_048,
            default_context: 4_096,
            threads: 0,
            disk_reserve_mb: 2_048,
            echo_model_enabled: false,
            gpu_fit_margin_mib: default_gpu_fit_margin_mib(),
        }
    }
}

impl InferenceSettings {
    /// The server validates these, but the client never trusts them blindly.
    pub fn sanitized(self) -> Self {
        InferenceSettings {
            progress_interval_ms: self.progress_interval_ms.max(MIN_PROGRESS_INTERVAL_MS),
            model_load_timeout_ms: self.model_load_timeout_ms.max(MIN_MODEL_LOAD_TIMEOUT_MS),
            max_concurrent: self.max_concurrent.clamp(1, 64),
            max_tokens: self.max_tokens.max(1),
            default_context: self.default_context.max(MIN_CONTEXT),
            threads: self.threads.min(1_024),
            gpu_fit_margin_mib: self.gpu_fit_margin_mib.min(1 << 20),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_are_checked() {
        for ok in ["echo", "qwen2.5-0.5b-instruct-q4_k_m", "a", "x.y_z-1", &"a".repeat(100)] {
            assert!(valid_model_id(ok), "{ok:?} should be valid");
        }
        for bad in [
            "",
            ".",
            "..",
            "...",
            "../etc",
            "a/b",
            "a\\b",
            "Qwen",
            "white space",
            "nul\0",
            "ünï",
            &"a".repeat(101),
        ] {
            assert!(!valid_model_id(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn sanitized_inference_settings_are_usable() {
        let s = InferenceSettings {
            progress_interval_ms: 0,
            model_load_timeout_ms: 0,
            max_concurrent: 0,
            max_tokens: 0,
            default_context: 1,
            threads: 100_000,
            ..InferenceSettings::default()
        }
        .sanitized();
        assert_eq!(s.progress_interval_ms, MIN_PROGRESS_INTERVAL_MS);
        assert_eq!(s.model_load_timeout_ms, MIN_MODEL_LOAD_TIMEOUT_MS);
        assert_eq!((s.max_concurrent, s.max_tokens), (1, 1));
        assert_eq!(s.default_context, MIN_CONTEXT);
        assert_eq!(s.threads, 1_024);
        assert_eq!(InferenceSettings::default().sanitized(), InferenceSettings::default());
    }

    #[test]
    fn llama_cpp_versions_parse_and_compare_by_build_number() {
        let v = |s| LlamaCppVersion::parse(s);
        assert_eq!(v("b12110"), Some(LlamaCppVersion(12110)));
        assert!(v("b999").unwrap() < v("b12110").unwrap(), "numeric, not textual");
        assert_eq!(v("b12110").unwrap().to_string(), "b12110");
        for bad in ["", "b", "12110", "B12110", "b12x", "b-1", "b1234567890", "b 1"] {
            assert_eq!(v(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn gpu_layers_and_settings_have_their_wire_forms() {
        for (layers, wire) in [
            (GpuLayers::Auto, "\"auto\""),
            (GpuLayers::Off, "\"off\""),
            (GpuLayers::All, "\"all\""),
            (GpuLayers::Count(20), "20"),
        ] {
            assert_eq!(serde_json::to_string(&layers).unwrap(), wire);
            assert_eq!(serde_json::from_str::<GpuLayers>(wire).unwrap(), layers);
        }
        assert!(serde_json::from_str::<GpuLayers>("\"most\"").is_err());
        assert!(serde_json::from_str::<GpuLayers>("-1").is_err());
        let s: ModelSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, ModelSettings::default());
        assert!(ModelSettings {
            context_length: Some(10),
            ..ModelSettings::default()
        }
        .problem()
        .is_some());
        assert!(ModelSettings {
            gpu_layers: GpuLayers::Count(MAX_GPU_LAYERS + 1),
            ..ModelSettings::default()
        }
        .problem()
        .is_some());
        assert_eq!(ModelSettings::default().problem(), None);
    }
}
