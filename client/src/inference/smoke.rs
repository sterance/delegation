//! The runtime-smoke check (`.github/workflows/runtime-smoke.yml`): the
//! pinned llama.cpp CPU build, fetched and run by the client's own code,
//! still does everything this client expects of it. It's how a weekly
//! llama.cpp bump is tested: ignored by default, since it downloads the
//! runtime (~25 MB) and a model (~500 MB). Run it with
//!
//! ```text
//! export DELEGATION_SMOKE_DIR=/tmp/smoke
//! eval "$(grep -E '^TEST_MODEL_(ID|URL|SHA256|SIZE_BYTES)=' config.env.example | sed 's/^/export /')"
//! cargo test -p delegation-client --lib smoke -- --ignored --nocapture
//! ```
//!
//! The model stays in `DELEGATION_SMOKE_DIR` between runs. The runtime is
//! downloaded fresh every time, so the pinned SHA-256 is really checked.
//! GPU builds can't run on CI's machines; the update PR has a checklist
//! for checking them by hand.

use super::llama_cpp::{self, GpuArgs, OFFLOADED_FORMAT, USING_DEVICE_FORMAT};
use super::*;
use protocol::{ChatRole, GpuLayers, GpuOffload, ModelMeta, ModelSource, ModelSpec};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name} to run the smoke test (see smoke.rs)"))
}

/// `fmt` with each `%…` conversion replaced by the next value.
fn printf(fmt: &str, values: &[&str]) -> String {
    let mut out = String::new();
    let mut values = values.iter();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        while chars.next_if(|c| !c.is_ascii_alphabetic()).is_some() {}
        while chars.next_if(|c| matches!(c, 'z' | 'l' | 'h')).is_some() {}
        chars.next();
        out.push_str(values.next().expect("a value per conversion"));
    }
    out
}

/// Whether `--help` lists `flag` (as itself, not as the start of a
/// longer flag).
fn has_flag(help: &str, flag: &str) -> bool {
    help.match_indices(flag).any(|(i, _)| {
        let before = help[..i].chars().next_back();
        let after = help[i + flag.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '-')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_')
    })
}

/// Every flag the client passes llama-server, whatever the settings.
fn client_flags() -> BTreeSet<String> {
    // runtime.rs's probe of a GPU build.
    let mut flags = BTreeSet::from(["--list-devices".to_string()]);
    let model = std::path::Path::new("model.gguf");
    for layers in [GpuLayers::Off, GpuLayers::All, GpuLayers::Count(10), GpuLayers::Auto] {
        for variant in ["cpu", "cuda-13"] {
            let gpu = GpuArgs {
                variant: variant.into(),
                layers,
                fit_margin_mib: 1024,
            };
            for arg in llama_cpp::server_args(model, 8080, 2048, 2, 4, &gpu) {
                if arg.starts_with('-') && arg.parse::<f64>().is_err() {
                    flags.insert(arg);
                }
            }
        }
    }
    flags
}

#[test]
fn the_helpers_work() {
    assert_eq!(
        printf("a %d/%d b %s - %zu MiB", &["1", "2", "x", "3"]),
        "a 1/2 b x - 3 MiB"
    );
    let help = "-c,    --ctx-size N\n-fit, --fit [on|off]\n-fitt, --fit-target MiB\n";
    assert!(has_flag(help, "--fit") && has_flag(help, "--fit-target") && has_flag(help, "-c"));
    assert!(!has_flag(help, "--ctx") && !has_flag(help, "-fi"));
    let flags = client_flags();
    for f in [
        "-m",
        "-c",
        "-np",
        "-ngl",
        "--fit",
        "--fit-target",
        "-t",
        "--offline",
        "--list-devices",
    ] {
        assert!(flags.contains(f), "{f} in {flags:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "downloads llama.cpp and a model; the runtime-smoke CI job runs it"]
async fn the_pinned_cpu_build_works_with_this_client() {
    let dir = PathBuf::from(env("DELEGATION_SMOKE_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    match std::fs::remove_dir_all(dir.join("runtime")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => panic!("couldn't clear the runtime: {e}"),
        _ => {}
    }
    let inf = Inference::new(&dir);
    inf.apply_settings(&InferenceSettings {
        disk_reserve_mb: 0,
        ..InferenceSettings::default()
    });
    // No GPU here: the CPU build is the only candidate.
    inf.shared.runtime.set_hardware(Vec::new());
    let version = inf.shared.runtime.version().to_string();
    println!("llama.cpp {version}");

    // 1. Install, as the server would ask: the runtime (downloaded,
    //    checked against runtime-pins.json, unpacked), then the model.
    let model_id = env("TEST_MODEL_ID");
    let spec = ModelSpec {
        id: model_id.clone(),
        display_name: model_id.clone(),
        backend: Backend::LlamaCpp,
        source: ModelSource {
            url: env("TEST_MODEL_URL"),
            sha256: env("TEST_MODEL_SHA256"),
            size_bytes: env("TEST_MODEL_SIZE_BYTES").parse().expect("TEST_MODEL_SIZE_BYTES"),
        },
        context_length: Some(2048),
        meta: ModelMeta::default(),
        min_runtime_version: None,
    };
    let (tx, mut rx) = mpsc::channel(256);
    inf.execute("smoke".into(), ModelCommandKind::Pull { model: spec }, tx);
    let last = loop {
        let s = tokio::time::timeout(Duration::from_secs(30 * 60), rx.recv())
            .await
            .expect("the install took over 30 minutes")
            .expect("no final status");
        if s.state.is_final() {
            break s;
        }
    };
    assert_eq!(
        (last.state, last.error.as_deref()),
        (CommandState::Done, None),
        "installing llama.cpp {version} (cpu) and the model"
    );
    let exe = inf
        .shared
        .runtime
        .server_exe("cpu")
        .expect("the CPU build is installed");
    let install = exe.parent().unwrap().to_path_buf();

    // 2. Every flag the client passes is still there.
    let out = std::process::Command::new(&exe)
        .arg("--help")
        .current_dir(&install)
        .output()
        .expect("llama-server --help");
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let missing: Vec<String> = client_flags().into_iter().filter(|f| !has_flag(&help, f)).collect();
    assert!(
        missing.is_empty(),
        "llama-server {version} --help no longer lists {missing:?} (server_args in llama_cpp.rs). Its help:\n{help}"
    );

    // 3. The offload log lines still parse. The CPU build never prints
    //    them, but its libllama is the same code as the GPU builds', so
    //    the formats must be in it.
    let mut libs = Vec::new();
    for entry in std::fs::read_dir(&install).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.contains("llama") && !path.is_symlink() && path.is_file() {
            libs.extend(std::fs::read(&path).unwrap());
        }
    }
    for fmt in [OFFLOADED_FORMAT, USING_DEVICE_FORMAT] {
        assert!(
            libs.windows(fmt.len()).any(|w| w == fmt.as_bytes()),
            "llama.cpp {version} no longer logs {fmt:?}: update parse_offloaded / parse_using_device in llama_cpp.rs"
        );
    }
    let offloaded = format!("load_tensors: {}", printf(OFFLOADED_FORMAT, &["25", "25"]));
    assert_eq!(
        llama_cpp::parse_offloaded(&offloaded),
        Some(GpuOffload {
            offloaded: 25,
            total: 25
        }),
        "{offloaded}"
    );
    let device = format!(
        "llama_model_load_from_file_impl: {}",
        printf(
            USING_DEVICE_FORMAT,
            &["CUDA0", "NVIDIA GeForce RTX 3070", "0000:01:00.0", "7347"]
        )
    );
    assert_eq!(
        llama_cpp::parse_using_device(&device).as_deref(),
        Some("NVIDIA GeForce RTX 3070"),
        "{device}"
    );

    // 4. A reply through the client's streaming code, with its timings.
    let (tx, mut events) = mpsc::channel(4096);
    let usage = tokio::time::timeout(
        Duration::from_secs(300),
        inf.chat(
            ChatCompletion {
                model_id: model_id.clone(),
                messages: vec![ChatTurn {
                    role: ChatRole::User,
                    content: "Count from 1 to 5.".into(),
                }],
                max_tokens: Some(16),
                temperature: Some(0.0),
            },
            tx,
        ),
    )
    .await
    .expect("no reply within 5 minutes")
    .expect("the chat failed");
    let mut text = String::new();
    while let Ok(event) = events.try_recv() {
        if let InferenceEvent::Delta(d) = event {
            text.push_str(&d);
        }
    }
    println!("reply: {text:?}\nusage: {usage:?}");
    assert!(!text.trim().is_empty(), "no text streamed");
    let usage = usage.expect("llama-server sent no timings");
    assert!(usage.prompt_tokens > 0 && usage.completion_tokens > 0, "{usage:?}");
    assert!(usage.tokens_per_second.is_some_and(|t| t > 0.0), "{usage:?}");
    assert!(usage.prompt_tokens_per_second.is_some_and(|t| t > 0.0), "{usage:?}");
    let run = usage.run.expect("no run info");
    assert_eq!(run.variant, "cpu");
    assert_eq!(run.device, None, "CPU only");

    inf.shutdown().await;
}
