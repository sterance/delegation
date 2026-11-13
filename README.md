# Delegation

Delegation runs large language models on computers that volunteers lend to
it, and gives people a web dashboard to watch those computers and chat with
the models on them.

A central **server** hosts the dashboard and the chat. Volunteer machines run
a small **client** that pairs with the server, reports its hardware and live
usage, installs models when the dashboard asks, and runs them locally with
[llama.cpp](https://github.com/ggml-org/llama.cpp). Chat replies stream from
the client, through the server, to the browser.

## What works today

**Accounts**
- Built-in accounts with sessions and an optional "Remember me". Passwords are
  hashed with Argon2id, and logins, sign-ups and pairing are rate-limited.
- The first visit to `/setup` creates the admin. Sign-up can be open or off.
- The admin's Users page manages accounts and makes password-reset links.

**Clients**
- *Add client* in the dashboard gives a one-time pairing code. A client
  enrolls with it, gets an Ed25519 keypair, and from then on signs in with
  that key.
- Each client shows as a card with:
  - whether it's connected and whether it's working on a job
  - live CPU, memory and GPU graphs
  - a hardware report: CPU, RAM, GPUs, disks, OS and swap
- Owners can rename or delete their clients. The admin sees everyone's.

**Models**
- A client card's *Models* section installs, cancels and deletes models.
  It shows download progress, disk use and the llama.cpp runtime's state.
- Models come from a server-side catalog: one test model, Qwen2.5 0.5B
  Instruct (Q4_K_M), set in `config.env`, plus any in the JSON file at
  `MODEL_CATALOG_PATH`. Browsers only ever send a catalog id: the URL and
  SHA-256 never come from the browser. An entry can name the oldest
  llama.cpp that runs it; older clients aren't offered it.
- Clients download over HTTPS only, resume interrupted downloads, and check
  each file's size and SHA-256. A download is refused if it would leave the
  disk without the reserve set in `config.env`.
- The first install also fetches llama.cpp. The release (currently
  `b12110`) and the hashes of its CPU, CUDA 12, CUDA 13 and Vulkan builds
  are pinned in the client (`client/runtime-pins.json`, compiled in), so
  the server can't make a client run a different executable. It can only
  choose among those builds.
- Clients use their GPUs. A client picks a build from its hardware (CUDA
  for NVIDIA, Vulkan for AMD and discrete Intel, CPU otherwise), checks it
  actually sees a GPU, and falls back to the next one, ending with CPU, if
  it doesn't or if it fails to load a model. The card shows which build is
  in use, its GPUs and any that failed, and lets you choose another.
- Models go to the GPU as far as the free VRAM allows (llama.cpp's
  `--fit`), or as set per model on the card (auto, CPU only, all layers or
  a number). The card shows how many layers ended up on which GPU, and
  chat shows each reply's speed in tokens per second.
- The client runs `llama-server` on `127.0.0.1` only, loads models on
  demand, and unloads them after an idle timeout to give the memory back.
- A built-in *echo* model, off by default, answers with your own message
  for testing.

**Chat**
- Conversations are stored on the server, with titles and branches:
  editing a message or retrying a reply makes a sibling you can switch
  between.
- Replies stream live and can be stopped. A reply survives a browser
  reload.
- The model picker lists the usable models on the clients you can use,
  grouped by client. "Loading model…" shows while a client loads one.
- Long conversations are trimmed from the oldest turns to fit the model's
  context. The newest message is always kept.

Not built yet:
- lending a client to other users
- a shared job queue ("any client with this model")
- picking models automatically
- ROCm, SYCL and Metal builds (AMD and Intel GPUs use Vulkan)
- choosing which GPUs a multi-GPU machine uses

## Layout

| Path | What it is |
|---|---|
| `protocol/` | The WebSocket messages shared by the server and client (Rust) |
| `server/` | The server: HTTP API, WebSocket hub, SQLite storage, serves the dashboard (Rust, axum) |
| `client/` | The client: a CLI for Linux/Docker, and a Windows tray app (Rust) |
| `frontend/` | The dashboard (React, MUI, Vite). Node is only needed to build it |
| `installer/` | The Windows installer (Inno Setup) and the code signing setup |
| `dev/` | Local development: `dev.sh`, a compose file, and notes |
| `config.env.example` | Every server setting, with its default and what it does |

The server reads all its settings from `config.env`. Clients have no
config file of their own: the server sends them its settings when they
connect. Rust is pinned in `rust-toolchain.toml` (1.99.1).

## Running the server

Copy `config.env.example` to `config.env` and set at least
`PUBLIC_ORIGIN`. Every key is optional and documented in the example file.
Unless every request reaches the server through Cloudflare, also set
`TRUST_CF_CONNECTING_IP=false`. Otherwise anyone can forge the header the
rate limits rely on.

**Docker:**

```sh
docker build -f server/Dockerfile -t delegation-server .
docker run -d -p 7070:7070 --env-file config.env -v delegation-data:/app/data delegation-server
```

**Natively** (needs Rust; Node only to build the dashboard):

```sh
(cd frontend && npm ci && npm run build)
cargo run --release -p delegation-server
```

Then open the site and create the admin at `/setup`. The database is SQLite,
and the server backs it up before upgrading its schema (`server/DATABASE.md`).

## Running a client

Get a pairing code from the dashboard's *Add client* button first.

**Windows:** install `DelegationTray-Setup-<version>.exe` from
[Releases](https://github.com/sterance/delegation/releases), or run the
portable exe. The tray app:
- asks for the server and pairing code
- remembers several servers and switches between them from its menu
- starts at login

It installs per-user, without admin rights.

**Linux, with Docker:**

```sh
cd client
DELEGATION_SERVER=https://your.server PAIRING_CODE=ABCD1234 docker compose up -d
```

Add `-f docker-compose.gpu.yml` (NVIDIA, needs the NVIDIA Container
Toolkit) or `-f docker-compose.amd.yml` (AMD/Intel, builds the image with
the Vulkan loader and Mesa's drivers) to report and use GPUs. Without
either, models run on the CPU. Its identity and models live in Docker
volumes, so they survive `down` and `up`.

**Linux, natively:**

```sh
cargo run --release -p delegation-client -- --server https://your.server --pairing-code ABCD1234
```

Later runs only need `--server`. Running models natively needs
glibc 2.34 or newer, `libssl3` and `libgomp1`. For GPUs:
- **NVIDIA:** only the NVIDIA driver. The CUDA runtime and cuBLAS come
  with the pinned build. Drivers for CUDA 13.4 get the CUDA 13 build
  (Turing, compute capability 7.5, and newer); drivers for CUDA 12.8 the
  CUDA 12 build.
- **AMD and Intel:** the Vulkan loader and a driver, e.g.
  `sudo apt install libvulkan1 mesa-vulkan-drivers`.

Without them the client says the GPU build found no GPU and uses the CPU.

**Windows:** the GPU driver provides Vulkan and the CUDA driver; the
pinned build brings the CUDA runtime. Nothing else to install.

Where a client keeps things:

| | Identity and keys | Models and llama.cpp |
|---|---|---|
| Linux | `~/.config/delegation-client` | `~/.local/share/delegation-client` |
| Windows | `%APPDATA%\delegation-client` | `%LOCALAPPDATA%\delegation-client` |
| Docker | `client-identity` volume | `client-models` volume |

On Windows the private keys are encrypted with DPAPI for the Windows user.
On Linux they're in files only their owner can read.

## Development

```sh
dev/dev.sh up        # build and start a dev server and client (Docker)
dev/dev.sh native    # run the client from source against it
cd frontend && npm run dev   # dashboard with hot reload on :5173
```

`dev/local-test.md` has the details. CI (`.github/workflows/ci.yml`)
checks formatting, runs clippy and the tests, lints and builds the
dashboard, and builds both Docker images. Pushes to `main` then build the
Windows installer and publish it as a release, and deploy the server.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd frontend && npm run lint && npm run build)
```

## Code signing policy

Free code signing provided by [SignPath.io](https://signpath.io), certificate
by [SignPath Foundation](https://signpath.org).

Signed: the Windows tray app (`delegation-tray.exe`) and its installer
(`DelegationTray-Setup-<version>.exe`), as published on this repository's
[Releases](https://github.com/sterance/delegation/releases) page.

SignPath signing starts once the SignPath Foundation accepts the project.
Until then, releases are signed with the project's own self-signed
certificate. That proves a file is what CI built, but Windows doesn't trust
the certificate, so it still shows "Unknown publisher". Each release's notes
say how it was signed, and self-signed releases include the public
certificate and its thumbprint.

Releases are built by GitHub Actions from this repository's `main` branch.
With SignPath, every signing request is approved by hand. Pull-request
builds are never signed. The CI and signing setup are in `.github/workflows/windows.yml` and
`installer/SIGNING.md`.

Team roles. This is a one-person project:

| Role | Members |
|---|---|
| Committers and reviewers | [Chris Smith](https://github.com/sterance) |
| Approvers | [Chris Smith](https://github.com/sterance) |

Outside contributions arrive as pull requests, and the committer reviews
each one before it's merged.

Privacy: see [Privacy](#privacy) for what the client and server send, and
to whom.

## Privacy

The client sends information only to the Delegation server its user pairs
it with, and to the download hosts below when that server asks it to
install something. It has no analytics or telemetry of its own.

What a client sends to its server:
- its hostname and operating system
- a hardware report (CPU, memory, GPUs, disks)
- live usage (CPU, memory, swap and GPU load)
- the list of models it has installed, and download progress
- the replies its models generate to the chat messages the server sends it

What it downloads, when its server asks it to install a model:
- **llama.cpp**, from the official release on github.com: the build for
  its hardware (for CUDA, about 0.6 to 0.8 GB with the CUDA runtime). The
  version and SHA-256s are fixed in the client.
- **model files**, from the URLs in the server's catalog. The default test
  model comes from huggingface.co.

Those hosts see an ordinary download request from the client's IP address.
Their own privacy policies apply ([GitHub](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement),
[Hugging Face](https://huggingface.co/privacy)). Models run locally, and
`llama-server` only listens on `127.0.0.1`.

The server stores accounts (usernames and Argon2id password hashes),
sessions, enrolled clients, and conversations, in its own SQLite database.
It sends none of it to anyone else. Conversations are kept until deleted, or
for `CHAT_RETENTION_DAYS` if the server sets it.

## License

[MIT](LICENSE) © 2026 Chris Smith
