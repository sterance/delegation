# Delegation Server — Networking Scaffold Implementation Guide

**Scope of this scaffold:** server core & infra, security/enrollment, and a
minimal Linux client agent that does nothing but enroll, connect, and
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
 │   Client Agent      │ ───────────────────────────────▶ │  Cloudflare Tunnel    │
 │  (Rust, Linux only) │                                   │  (llm.smith-c.com)   │
 └────────────────────┘                                   └──────────┬───────────┘
                                                                       │
                                                            ┌──────────▼───────────┐
                                                            │  Delegation Server    │
                                                            │  (Node/TS, Docker)    │
                                                            │  - /enroll  (HTTP)    │
                                                            │  - /agent   (WS)      │
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
└── client/                  # Rust client agent (Linux-only for now)
    ├── src/
    │   ├── main.rs          # connect, handshake, heartbeat loop, reconnect/backoff
    │   ├── identity.rs      # keypair generation, persistence, enrollment
    │   └── protocol.rs      # mirrors server/src/protocol.ts by hand
    ├── Cargo.toml
    ├── Dockerfile           # multi-stage: rust:1-slim-bookworm → debian:bookworm-slim
    ├── docker-entrypoint.sh
    └── docker-compose.yml   # for running the agent itself in a container
```

### Shared configuration

The project-wide [config.env.example](./config.env.example) file is the
tracked template for the shared dotenv-style configuration used by both
services. Before deploying, create the ignored runtime file:

```bash
cp config.env.example config.env
```

The server package scripts load `config.env` automatically, and both
Compose files pass it to their respective services. Duration values use
milliseconds:

| Variable                              |  Default | Purpose                                                      |
| ------------------------------------- | -------: | ------------------------------------------------------------ |
| `SERVER_AUTH_TIMEOUT_MS`              |   `5000` | Time allowed for the WebSocket handshake                     |
| `SERVER_HEARTBEAT_TIMEOUT_MS`         |  `30000` | Time without a heartbeat before closing a client             |
| `SERVER_HEARTBEAT_SWEEP_INTERVAL_MS`  |  `10000` | Frequency of the server heartbeat timeout sweep              |
| `SERVER_PAIRING_CODE_TTL_MS`          | `600000` | Pairing-code validity period                                 |
| `CLIENT_ACK_TIMEOUT_MS`               |  `30000` | Time without a heartbeat acknowledgement before reconnecting |
| `CLIENT_HEARTBEAT_INTERVAL_MS`        |  `10000` | Interval between client heartbeats                           |
| `CLIENT_RECONNECT_INITIAL_BACKOFF_MS` |   `1000` | Initial reconnect delay                                      |
| `CLIENT_RECONNECT_MAX_BACKOFF_MS`     |  `30000` | Maximum reconnect delay                                      |

`SERVER_PORT` remains the server listening port. Client configuration can be
added to the same file as the client gains additional settings.

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
   `~/.config/delegation-agent/identity.json`, permissions `0600`. This
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
  │◀──── {type: "auth_ok"} ────────────│
```

This is deliberately WireGuard/SSH-style: the private key never leaves the
client, ever — not even during enrollment, where only the _public_ key is
sent. Possession of the private key is proven by signing a fresh
server-issued nonce, so a captured message can't be replayed.

### 3.3 Heartbeat (every `CLIENT_HEARTBEAT_INTERVAL_MS` once authenticated)

```
client ── {type:"heartbeat", timestamp, hostname} ──▶ server
client ◀──────── {type:"heartbeat_ack"} ─────────────── server
```

The server sweeps every `SERVER_HEARTBEAT_SWEEP_INTERVAL_MS` for any
authenticated client whose last heartbeat is older than
`SERVER_HEARTBEAT_TIMEOUT_MS` and closes that connection, logging it as
timed out.

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
#   agent:   ws://localhost:7070/agent

# in another terminal, mint a pairing code:
npm run gen-code
#   Pairing code (valid according to SERVER_PAIRING_CODE_TTL_MS): ABC12345
```

```bash
# --- client (native binary) ---
cd client
cargo build --release
./target/release/delegation-agent --server http://localhost:7070 --pairing-code ABC12345
#   [identity] enrolling with http://localhost:7070/enroll ...
#   [identity] enrolled, client_id=...
#   [ws] connecting to ws://localhost:7070/agent ...
#   [ws] authenticated
#   [heartbeat] sent / acked, according to CLIENT_HEARTBEAT_INTERVAL_MS
```

Run it again later with no `--pairing-code` at all — it loads the saved
identity and reconnects straight to the heartbeat loop.

### 4.1 Running the client via Docker instead

```bash
cd client
echo "DELEGATION_SERVER=http://host.docker.internal:7070" > .env
echo "PAIRING_CODE=ABC12345" >> .env
docker compose up --build
```

The identity persists in a named Docker volume (`agent-identity`), so
`docker compose up` again later — even after removing and recreating the
container — reconnects with the same identity rather than re-enrolling.
Once enrolled, `PAIRING_CODE` in `.env` is harmless to leave in place; it's
single-use server-side and only ever read if no identity file exists yet.

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
   and enroll a real client agent against `https://llm.smith-c.com` instead
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

**The fix:** the client needs its own application-level liveness check —
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

1. Fix the heartbeat-ack staleness gap from Section 6, item 3 — this is
   the one real correctness bug found here.
2. Run `docker compose build` for both server and client on a real
   machine with Docker Hub access, and fix whatever that first real build
   surfaces.
3. Set up the actual Cloudflare Tunnel and confirm the exact same protocol
   works unmodified against `llm.smith-c.com` instead of `localhost`.
4. Only then: start layering the job queue, model suggestion engine, and
   dashboard on top of a networking layer you've now actually stress-tested.
