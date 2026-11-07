# Delegation Server — Networking Scaffold Implementation Guide

**Scope of this scaffold:** server core & infra, security/enrollment, and a
minimal Linux client that does nothing but enroll, connect, and
heartbeat. No job queue, no inference, no dashboard, no Windows/macOS
builds — all of that comes later. The entire point of this stage is to
prove the networking and security model actually works before anything
else is built on top of it, and to surface real problems while they're
still cheap to fix.

Everything in this guide was actually built and tested while writing it —
see [Section 6](#6-hurdles-encountered-and-what-they-mean-for-you) for what
broke and what that implies for the full build.

---

## 1. Architecture recap

```
 ┌────────────────────┐        outbound WSS/HTTPS         ┌──────────────────────┐
 │   Client            │ ───────────────────────────────▶ │  Cloudflare Tunnel    │
 │  (Rust, Linux only) │                                   │  (llm.smith-c.com)   │
 └────────────────────┘                                   └──────────┬───────────┘
                                                                       │
                                                            ┌──────────▼───────────┐
                                                            │  Delegation Server    │
                                                            │  (Node/TS, Docker)    │
                                                            │  - /enroll  (HTTP)    │
                                                            │  - /ws      (WS)      │
                                                            │  - file-backed store  │
                                                            └───────────────────────┘
```

The client never opens a listening port and never needs to know the
server's network location beyond its public domain — it just makes an
outbound connection, the same way a browser does. Only the server needs
the Cloudflare Tunnel, since it's the side that must be reachable.

---

## 2. Repository layout

```
scaffold/
├── server/                  # Node.js + TypeScript coordinator
│   ├── src/
│   │   ├── server.ts        # HTTP enroll endpoint + WebSocket handshake/heartbeat
│   │   ├── registry.ts      # file-backed client + pairing-code storage
│   │   ├── protocol.ts      # shared TS message types
│   │   └── gen-code.ts      # admin CLI: mint a one-time pairing code
│   ├── package.json / tsconfig.json
│   ├── Dockerfile           # node:20-alpine, multi-stage
│   └── docker-compose.yml   # server + cloudflared, for real deployment
│
└── client/                  # Rust client (Linux-only for now)
    ├── src/
    │   ├── main.rs          # connect, handshake, heartbeat loop, reconnect/backoff
    │   ├── identity.rs      # keypair generation, persistence, enrollment
    │   └── protocol.rs      # mirrors server/src/protocol.ts by hand
    ├── Cargo.toml
    ├── Dockerfile           # multi-stage: rust:1-slim-bookworm → debian:bookworm-slim
    ├── docker-entrypoint.sh
    └── docker-compose.yml   # for running the client itself in a container
```

### Configuration (`config.env`)

Every setting lives in one dotenv-style file, `config.env` at the repo root
(gitignored). [config.env.example](../config.env.example) is the tracked
template, with every variable, its default and a one-line comment:

```bash
cp config.env.example config.env
```

- **Only the server reads it.** Compose passes it to the server container
  with `env_file` (`server/docker-compose.yml`, `dev/docker-compose.yml`); a
  native server reads `$DELEGATION_CONFIG` or `./config.env`. A real
  environment variable overrides the file.
- **Every variable is optional**, with its default compiled in. A blank
  value counts as unset.
- **Parsed and validated once at startup**, including the rules between
  variables. Any invalid value stops the server with a line naming the
  variable and the rule it broke (`[config] invalid: ...`); nothing falls
  back silently. The effective configuration is logged at startup.
  Variables from earlier versions (`SERVER_HEARTBEAT_*`, `CLIENT_*`,
  `SERVER_PAIRING_CODE_TTL_MS`) are ignored with a warning naming their
  replacement.
- **Client settings are pushed by the server.** Clients run on other
  machines (the Windows tray has no `config.env`), so the variables marked
  *pushed* below are sent to each client in `welcome` when it connects.
  Change them in `config.env`, restart the server, and clients pick them up
  as they reconnect. A client caches the last set it received (the CLI in
  `server-settings.json` next to `identity.json`; the tray per server in
  `servers.json`) so its reconnect backoff follows the server even before it
  has connected; before its first connection it uses the compiled defaults.
- **The dashboard reads its settings at runtime** from
  `GET /admin/dashboard-config`, so changing a `DASHBOARD_*` value needs a
  server restart, not a frontend rebuild.

> **ANTON:** the deployed server reads `~/delegation/config.env` on ANTON, a
> separate copy that deploys never touch. Add the new variables there (or
> leave them out to run on the defaults), and delete the old
> `SERVER_HEARTBEAT_*` / `CLIENT_*` / `SERVER_PAIRING_CODE_TTL_MS` lines,
> which now only produce startup warnings.

**Server**

| Variable | Default | Meaning |
|---|---:|---|
| `SERVER_PORT` | `7070` | Listening port. The compose mapping, `dev/dev.sh` and the tunnel route assume 7070 |
| `SERVER_AUTH_TIMEOUT_MS` | `5000` | Time allowed for each handshake step |
| `SERVER_LOG_LEVEL` | `info` | `debug` adds a line per tick, per sample and per flush |
| `DB_PATH`, `FRONTEND_DIR` | `data/delegation.db`, `frontend/dist` | Native-run paths; the Docker image sets its own |

**Tick and liveness**

| Variable | Default | Applies to | Meaning |
|---|---:|---|---|
| `TICK_INTERVAL_MS` | `1000` | pushed | How often the client sends a `tick` |
| `SAMPLER_INTERVAL_MS` | `500` | pushed | How often the client's sampler takes a reading. At least 200 |
| `SAMPLE_MAX_AGE_MS` | `3000` | pushed | A tick carries no metrics if the newest reading is older. At least 2 × `SAMPLER_INTERVAL_MS` |
| `LIVENESS_TIMEOUT_MS` | `4000` | server | No frame from a client for this long and the server drops it. At least 2 × `TICK_INTERVAL_MS` |
| `WS_PING_INTERVAL_MS` | `2000` | server | How often the server pings each client |
| `SERVER_SILENCE_TIMEOUT_MS` | `6000` | pushed | Nothing from the server for this long and the client reconnects. At least 2 × `WS_PING_INTERVAL_MS` |
| `RECONNECT_BACKOFF_INITIAL_MS` | `1000` | pushed, cached | First reconnect delay; doubles per failure |
| `RECONNECT_BACKOFF_MAX_MS` | `30000` | pushed, cached | Reconnect delay cap. At least the initial delay |
| `LAST_SEEN_FLUSH_INTERVAL_MS` | `30000` | server | How often in-memory `last_seen_at` values are written to SQLite |

**Metrics**

| Variable | Default | Meaning |
|---|---:|---|
| `METRICS_HISTORY_SAMPLES` | `60` | Samples kept per client in memory; also each graph's width in samples. At least 2 |
| `METRICS_MIN_SPACING_MS` | `500` | Samples from one client closer together than this are dropped. Less than `TICK_INTERVAL_MS`; 0 disables |
| `METRICS_MAX_GPUS` | `16` | Samples listing more GPUs are dropped |
| `METRICS_MAX_STRING_LEN` | `128` | Samples with a longer string (GPU name) are dropped |

**Pairing**

| Variable | Default | Meaning |
|---|---:|---|
| `PAIRING_CODE_TTL_SECS` | `600` | Pairing code validity; the Add Client dialog's "valid for" text follows it |
| `DASHBOARD_PAIRING_POLL_INTERVAL_MS` | `1500` | How often the Add Client dialog checks its code |

**Dashboard**

| Variable | Default | Meaning |
|---|---:|---|
| `DASHBOARD_POLL_INTERVAL_MS` | `1000` | How often the dashboard polls `GET /clients` |
| `DASHBOARD_PAUSE_WHEN_HIDDEN` | `true` | Stop polling while the tab is hidden; poll at once when it's shown |
| `DASHBOARD_GRAPH_WIDTH_PX` / `_HEIGHT_PX` / `_GRID_PX` | `120` / `40` / `10` | Usage graph size and grid cell |
| `DASHBOARD_GRAPH_LINE_COLOR` / `_GRID_COLOR` / `_BG_COLOR` | `#00ff00` / `#008040` / `#000000` | Graph colours, `#rgb` or `#rrggbb`, unquoted (`docker run --env-file` would keep quotes) |
| `DASHBOARD_COLLAPSED_MAX_GPUS` | `1` | GPUs graphed in a collapsed card; the rest show when it's expanded |
| `DASHBOARD_BLINK_PERIOD_MS` | `1000` | One full bright/dim cycle of the blue dot beside each card's status dot |

With the defaults, a value on screen is typically under a second old, and
up to about 1.5–2 s in the worst case, plus network latency. Lower
`SAMPLER_INTERVAL_MS`, `TICK_INTERVAL_MS` and `DASHBOARD_POLL_INTERVAL_MS`
to tighten that.

---

## 3. Protocol specification

### 3.1 Enrollment (one-time, over HTTP)

1. On the server: `npm run gen-code` prints a random 8-character code, valid
   10 minutes, single-use.
2. The client generates an Ed25519 keypair locally (if it doesn't already
   have one) and calls:

   ```
   POST /enroll
   { "pairing_code": "ABC12345", "public_key": "<base64>", "hostname": "..." }
   ```

3. The server validates and consumes the code, generates a `client_id`
   (UUID), stores `{client_id, public_key, hostname}`, and returns
   `{ "client_id": "..." }`.
4. The client saves `{client_id, private_key}` to
   `~/.config/delegation-client/identity.json`, permissions `0600`. This
   file is the client's entire identity — treat it like an SSH private key.

### 3.2 Connection handshake (every time the client connects)

```
client                              server
  │──── {type: "hello", client_id} ───▶│
  │                                     │  looks up client_id's public key
  │◀── {type: "challenge", nonce} ─────│  (16 random bytes, base64)
  │                                     │
  │  signs nonce with private key       │
  │─ {type:"challenge_response",       │
  │      signature} ───────────────────▶│  verifies signature against
  │                                     │  the stored public key
  │◀─ {type:"welcome", settings} ──────│
```

`welcome` is the auth-success message. `settings` is `ClientSettings`
(`protocol/src/telemetry.rs`): `tick_interval_ms`, `sampler_interval_ms`,
`sample_max_age_ms`, `server_silence_timeout_ms`,
`reconnect_backoff_initial_ms` and `reconnect_backoff_max_ms`, from the
server's `config.env`. The client applies them for the rest of the
connection and sends no ticks until `welcome` has arrived.

This is deliberately WireGuard/SSH-style: the private key never leaves the
client, ever — not even during enrollment, where only the _public_ key is
sent. Possession of the private key is proven by signing a fresh
server-issued nonce, so a captured message can't be replayed.

### 3.3 Ticks and liveness (once `welcome` has arrived)

```
client ── {type:"tick", seq, metrics} ──▶ server     every TICK_INTERVAL_MS, never acked
client ◀───────── WebSocket ping ──────── server     every WS_PING_INTERVAL_MS
client ────────── WebSocket pong ───────▶ server     sent automatically by tungstenite
```

There is no heartbeat or ack. The tick is both the liveness signal and the
carrier for resource metrics:

```json
{"type":"tick","seq":41,"metrics":{"sample_age_ms":212,"cpu_percent":37.2,
 "ram_total_bytes":33554432000,"ram_available_bytes":21474836480,
 "swap_total_bytes":8589934592,"swap_used_bytes":0,
 "gpus":[{"name":"NVIDIA GeForce RTX 3070","utilisation_percent":63.0,
          "vram_used_bytes":6549825536,"vram_total_bytes":8589934592,"power_watts":142.5}]}}
```

- `seq` counts up from 0 per connection, for debugging and spotting drops.
- `metrics` is `null` when the client has no reading newer than
  `SAMPLE_MAX_AGE_MS`. The tick is sent anyway.
- Metrics come from a **sampler thread** in the client library
  (`client/src/sampler.rs`), decoupled from the tick: it owns a long-lived
  `sysinfo::System` and the NVML handle and publishes its latest reading
  every `SAMPLER_INTERVAL_MS`. The tick loop only copies that reading, so a
  slow GPU query can never delay a tick or cause a false disconnect. RAM is
  reported as *available* (Linux `MemAvailable`), not "free". GPUs are
  NVIDIA only, through NVML loaded at runtime; without the driver the
  client logs that once and sends an empty `gpus` list.
- The tick timer doesn't drift, and after a stall it skips the missed ticks
  rather than sending a burst.

**Server side:** any inbound frame (tick, pong, even one that fails to
parse) is proof of life. A watchdog per connection checks every
`TICK_INTERVAL_MS`; after `LIVENESS_TIMEOUT_MS` without a frame it closes
the connection, marks the client disconnected, clears its sample history
and writes its `last_seen_at` to SQLite immediately. A client that
reconnects while its old connection is still open replaces it.

**Client side:** the client tracks the last frame of any kind from the
server, pings included. After `SERVER_SILENCE_TIMEOUT_MS` of silence it
closes the connection and reconnects with exponential backoff from
`RECONNECT_BACKOFF_INITIAL_MS` up to `RECONNECT_BACKOFF_MAX_MS`. This is the
fix for the silent-drop problem in §6, item 3.

**`last_seen_at`** is updated in memory on every inbound frame and written
to SQLite (changed rows only, one transaction) every
`LAST_SEEN_FLUSH_INTERVAL_MS`, on disconnect, and on graceful shutdown
(SIGTERM from `docker stop`, or Ctrl-C). `GET /clients` returns the
in-memory value, so it's current between flushes.

### 3.4 Client info (once per connection, after `welcome`)

```
client ── {type:"info", hostname, os, client_version, specs} ──▶ server
```

Not acked. The server stores it on the client's row (the hostname replaces
the one from enrollment, so renamed machines update) and `/clients`
returns it for the dashboard. `specs` is `HardwareSpecs` from
`protocol/src/hardware.rs`: CPU, RAM, GPUs and disks, every field optional
and in raw units. If the client's hardware scan is still running when it
authenticates, `info` follows as soon as the scan finishes.

### 3.5 Metrics on the server, and the dashboard API

Metrics are untrusted client input. The server drops a sample (logging
it at debug level; the connection is never affected) if a percentage is
outside 0–100, any number is NaN or infinite, `gpus` is longer than
`METRICS_MAX_GPUS`, a string is longer than `METRICS_MAX_STRING_LEN`, a used
value exceeds its total, or it arrives less than `METRICS_MIN_SPACING_MS`
after the previous accepted sample from that client.

Accepted samples go into an in-memory ring buffer per connected client
(`METRICS_HISTORY_SAMPLES` long), stamped with the server's receipt time and
a server-wide sequence number (one counter across all clients). Nothing
metrics-related is written to SQLite. A random **epoch** is generated at
startup so the dashboard can tell the sequence numbers have reset.

`GET /clients?since=<seq>` returns:

```json
{ "epoch": "3f9c…", "latest_seq": 48213,
  "clients": [ { "client_id": "…", "hostname": "lappy", "friendly_name": "lappy",
                 "enrolled_at": "…", "last_seen_at": "…", "connected": true,
                 "os": "…", "client_version": "…", "specs": {…}, "info_updated_at": "…",
                 "samples": [ { "seq": 48211, "received_at": "2026-11-06T12:01:08.412Z",
                                "cpu_percent": 37.2, "ram_total_bytes": …, "ram_available_bytes": …,
                                "swap_total_bytes": …, "swap_used_bytes": …, "gpus": [ … ] } ] } ] }
```

`samples` holds only samples with `seq > since`, oldest first; with `since`
missing or 0 it's each client's whole buffer, so graphs fill at once on
page load. A disconnected client's `samples` is empty.

`GET /admin/dashboard-config` returns the `DASHBOARD_*` values plus
`metrics_history_samples`, `tick_interval_ms` and `pairing_code_ttl_secs`.

The dashboard polls `/clients?since=<latest_seq>` every
`DASHBOARD_POLL_INTERVAL_MS`, each poll scheduled after the previous one
finishes, and keeps a per-client buffer for its usage graphs. A changed
`epoch` (or a `latest_seq` going backwards) means the server restarted, so
it clears its buffers and re-polls with `since=0`.

---

## 4. Running it locally

```bash
# --- server ---
cd server
npm install
npm run build
npm start
#   listening on :7070
#   enroll:  POST http://localhost:7070/enroll
#   ws:      ws://localhost:7070/ws

# in another terminal, mint a pairing code:
npm run gen-code
#   Pairing code (valid according to SERVER_PAIRING_CODE_TTL_MS): ABC12345
```

```bash
# --- client (native binary) ---
cd client
cargo build --release
./target/release/delegation-client --server http://localhost:7070 --pairing-code ABC12345
#   [identity] enrolling with http://localhost:7070/enroll ...
#   [identity] enrolled, client_id=...
#   [ws] connecting to ws://localhost:7070/ws ...
#   [ws] authenticated
#   [ws] server settings: tick 1000 ms, sampler 500 ms, ...
#   (ticks aren't logged; SERVER_LOG_LEVEL=debug shows them on the server)
```

Run it again later with no `--pairing-code` at all — it loads the saved
identity and reconnects straight to ticking.

### 4.1 Running the client via Docker instead

```bash
cd client
echo "DELEGATION_SERVER=http://host.docker.internal:7070" > .env
echo "PAIRING_CODE=ABC12345" >> .env
docker compose up --build
```

The identity persists in a named Docker volume (`client-identity`), so
`docker compose up` again later — even after removing and recreating the
container — reconnects with the same identity rather than re-enrolling.
Once enrolled, `PAIRING_CODE` in `.env` is harmless to leave in place; it's
single-use server-side and only ever read if no identity file exists yet.

The container sees the host's CPU and RAM (`/proc` isn't namespaced), which
is what the metrics want; a container memory limit doesn't show up in them.

**NVIDIA GPU metrics** need the NVIDIA Container Toolkit on the host and the
`utility` driver capability, which mounts NVML into the container. That
would break hosts without a GPU, so it's a separate override file rather
than the default:

```bash
docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d
```

Without it the client runs normally and reports CPU and RAM only.

---

## 5. Deploying for real, at llm.smith-c.com

1. **Cloudflare Tunnel:** in the Cloudflare Zero Trust dashboard, Networks
   → Tunnels → Create a tunnel → Docker. Point the public hostname
   `llm.smith-c.com` at `http://server:7070` (the compose service name —
   cloudflared and the server share the compose network, so this resolves
   without any port being published to the host). Copy the tunnel token.
2. On the server host (Pi or otherwise):
   ```bash
   cd server
   echo "TUNNEL_TOKEN=<paste token>" > .env
   docker compose up -d --build
   ```
3. Mint a pairing code on the server (`docker compose exec server node dist/gen-code.js`),
   and enroll a real client against `https://llm.smith-c.com` instead
   of `http://localhost:7070`. Everything else about the protocol is
   identical — only the URL changes.
4. **Alpine/Pi note:** the server's Dockerfile is pinned to pure-JS
   dependencies specifically so `npm install` doesn't need a native build
   toolchain under Alpine's musl libc. If you add any dependency later,
   check whether it has native (node-gyp) components before assuming it'll
   "just work" on the Pi the way it does on your dev machine.

---

## 6. Hurdles encountered, and what they mean for you

These are real problems hit while building and testing this exact
scaffold — not hypothetical risks.

**1. `@noble/ed25519` v2's documented API doesn't match what installs today.**
Most example code shows manually wiring up `ed.hashes.sha512` from a
separate `@noble/hashes` import. The version that actually installs
(2.3.0) already ships a working `etc.sha512Async` hook with no extra
dependency needed. Minor, but exactly the kind of stale-docs trap that
burns an hour if you don't double check the installed API directly.

**2. A distro-packaged Rust toolchain is too old for a modern dependency
tree, fast.** Building this on Ubuntu's `apt`-provided `rustc` 1.75
(itself from Dec 2023) failed repeatedly as unrelated crates deep in the
dependency tree (`clap`, `base64ct`, `idna`/`icu`, `zeroize`) had all since
moved to require Rust's 2024 edition. It cascaded — pinning one crate just
revealed the next. **Install Rust via `rustup`, not your OS package
manager**, on every machine that builds this client natively. This
scaffold's `Cargo.toml` has several version pins that exist only to work
around that constrained environment; a `rustup`-installed current stable
almost certainly doesn't need them.

- One genuine improvement came out of chasing this down anyway: the
  original design used `reqwest` for the one HTTP enroll call, which
  pulled in a large chunk of that dependency tree. Switching to `ureq`
  (synchronous, minimal deps) for just that one call meaningfully
  shrunk the client's footprint — a better fit for "as lightweight as
  possible" regardless of the toolchain issue.

**3. The server correctly detects a dead client — the client does not
symmetrically detect a dead server, and this matters a lot.** Three
distinct disconnect scenarios were tested:

- _Server killed outright (`kill -9`):_ the client sees an immediate
  "connection reset" error and reconnects with exponential backoff
  (1s→2s→4s→8s...), re-authenticating cleanly once the server comes
  back. **Works exactly as designed.**
- _Client killed outright, on localhost:_ the server sees an immediate
  clean disconnect, because the kernel still sends a proper close on
  the client's socket even under `kill -9`. This is **not** representative
  of a real network failure.
- _Packets silently dropped in both directions_ (simulated with
  `iptables`, no FIN/RST either way — this is what a dead Wi-Fi
  connection or a phone leaving range actually looks like): the server's
  30-second heartbeat-timeout sweep worked exactly as designed and
  correctly gave up on the client. **The client, however, kept sending
  heartbeats into the void indefinitely**, with no way to know the
  server had already forgotten about it — because the client only reacts
  to transport-level errors, and a silent packet drop produces none.
  Recovery only happened once connectivity was restored and a stale
  reset packet finally got through — which is not guaranteed on a real
  network (e.g. a device that gets a new IP address when it reconnects
  may never receive that stale packet at all, leaving it to Linux's
  default TCP retransmission timeout, which can exceed ten minutes).

**The fix** (now implemented: server pings plus the client's
`SERVER_SILENCE_TIMEOUT_MS`, see §3.3): the client needs its own
application-level liveness check —
track the time since the last `heartbeat_ack` was actually received, and
if it exceeds some multiple of the heartbeat interval (e.g. 3×), treat
the connection as dead and force a reconnect, rather than waiting on the
transport layer to notice. This is a small, well-understood change (the
same idea as WebSocket ping/pong timeouts) but it's the single most
important fix to make before relying on this over real, unreliable
client internet connections — it was not part of the original design and
would not have been found without this exact test.

**4. Docker Hub was unreachable from the sandboxed environment used to
prototype this**, so the Dockerfiles here are correct by careful,
tested-in-parts construction (the entrypoint script's logic was verified
directly; both Dockerfiles follow a deliberately-matched-base-image
pattern to avoid glibc mismatches between build and runtime stages) but
were **not** build-tested end-to-end the way the native binaries were.
Run `docker compose build` yourself as the first real step — treat it with
the same skepticism as everything else in this section until you've seen
it work.

---

## 7. What this scaffold deliberately does _not_ handle yet

- No dashboard (React/TypeScript, later) — the admin flow is a CLI script
  on purpose, so the enrollment protocol itself gets proven before any UI
  is built on top of it.
- No job queue, task execution, or inference — purely connect + heartbeat.
- No Windows or macOS client — Linux only, using Unix file permission bits
  directly for the identity file.
- No rate-limiting or abuse protection on `/enroll` (fine behind a
  10-minute single-use code for now; revisit if this is ever exposed
  beyond people you've personally handed a code to).
- No encryption of the identity file at rest beyond OS file permissions —
  matches the security model's threat assumptions (protecting against
  network attackers, not a compromised client machine) but worth stating
  explicitly rather than leaving implicit.

## 8. Recommended next steps, in order

1. ~~Fix the heartbeat-ack staleness gap from Section 6, item 3~~ — done:
   the server pings every client and the client reconnects after
   `SERVER_SILENCE_TIMEOUT_MS` of silence (§3.3).
2. Run `docker compose build` for both server and client on a real
   machine with Docker Hub access, and fix whatever that first real build
   surfaces.
3. Set up the actual Cloudflare Tunnel and confirm the exact same protocol
   works unmodified against `llm.smith-c.com` instead of `localhost`.
4. Only then: start layering the job queue, model suggestion engine, and
   dashboard on top of a networking layer you've now actually stress-tested.
