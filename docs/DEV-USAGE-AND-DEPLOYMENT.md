# Dev Usage and Deployment Guide

How to run and test the whole stack on a dev machine, then deploy the
server and clients onto real hardware.

The repo has four parts:

| Path        | What it is                                                                        |
| ----------- | --------------------------------------------------------------------------------- |
| `server/`   | Rust (axum) coordinator: enrollment, WebSocket auth and heartbeats, SQLite store  |
| `client/`   | Rust client library with two front ends: the headless `delegation-client` (Linux, Docker) and the `delegation-tray` Windows tray app. Enrolls once, then keeps an authenticated WebSocket open and reports the machine's hardware |
| `protocol/` | Message types shared by the server and client                                      |
| `frontend/` | React + MUI dashboard. Built into static files that the server binary serves       |

In production the client only ever makes outbound connections. The server
is reachable through a Cloudflare Tunnel, so it never opens a port on the
host.

---

## Part 1: Local development

### 1.1 Prerequisites (dev machine)

| Tool                          | Why                                         | Check                    |
| ----------------------------- | ------------------------------------------- | ------------------------ |
| Docker Engine + Compose v2    | Dev stack, production images                | `docker compose version` |
| `curl`, `jq`                  | Used by `dev/dev.sh`                        | `jq --version`           |
| Rust via **rustup** (stable)  | Native builds, `dev/dev.sh native`          | `cargo --version`        |
| A C compiler (`build-essential` / `gcc`) | Bundled SQLite (server) and `ring` (TLS) | `cc --version`      |
| Node.js 20.19+ or 22.12+      | Frontend dev server and builds              | `node --version`         |

Install Rust with [rustup](https://rustup.rs), **not** your distro's
package manager. Distro `rustc` is usually too old for this dependency tree.

### 1.2 One-command smoke test

From the repo root:

```bash
dev/dev.sh up
```

This command:

1. builds the server image (frontend included) and starts it on
   `127.0.0.1:7070` only
2. waits until `/clients` responds
3. mints a pairing code, then builds and starts a containerized client that
   enrolls with it (later runs skip enrollment if the client already has an
   identity)
4. prints `GET /clients`

You should see your client listed with `"connected": true`.

The dev stack uses the Compose project name `delegation-dev` and has its own
volumes, so nothing here touches production data or identities.

### 1.3 Everyday commands

```bash
dev/dev.sh status          # enrolled clients and whether each is connected
dev/dev.sh logs            # follow server + client logs
dev/dev.sh restart-server  # bounce the server; watch the client back off and reconnect
dev/dev.sh native          # run the client from source (cargo run) against the dev server
dev/dev.sh down            # stop containers, keep DB and identity
dev/dev.sh reset           # stop and wipe the dev DB and client identity
```

`dev/dev.sh native` stores its identity in `dev/.native-home/` (gitignored),
so it never overwrites a real `~/.config/delegation-client/identity.json`.
Each run mints a new pairing code, but once an identity exists the code is
ignored. Delete `dev/.native-home/` to enroll from scratch.

### 1.4 Frontend development

With the dev server running (`dev/dev.sh up`):

```bash
cd frontend
npm install
npm run dev        # http://localhost:5173, hot reload, /clients and /admin proxied to :7070
```

Before committing:

```bash
npm run lint       # oxlint
npm run build      # tsc type-check + production build into frontend/dist
```

To see the dashboard exactly as production serves it (from the Rust
binary), open http://127.0.0.1:7070/. That copy only updates when you rerun
`dev/dev.sh up`, which rebuilds the image.

### 1.5 Running everything natively (no Docker)

Useful for debugging with a profiler or debugger:

```bash
# terminal 1: server (run from repo root so the relative paths resolve)
(cd frontend && npm install && npm run build)
cargo run -p delegation-server
#   DB:        data/delegation.db   (override with DB_PATH)
#   Dashboard: frontend/dist        (override with FRONTEND_DIR)
#   Port:      7070                 (override with SERVER_PORT)
#   Settings:  ./config.env if present (see §1.8)

# terminal 2: mint a code and enroll a client
CODE=$(curl -fsS -X POST http://127.0.0.1:7070/admin/pairing-code | jq -r .code)
HOME=$PWD/dev/.native-home cargo run -p delegation-client -- \
  --server http://127.0.0.1:7070 --pairing-code "$CODE"
```

The native server binds `0.0.0.0`, so it's reachable from your LAN. Its admin
endpoints have no authentication. On an untrusted network, use the Docker
dev stack instead, since it only binds to localhost.

### 1.6 Manual API checks

```bash
curl -s -X POST http://127.0.0.1:7070/admin/pairing-code | jq          # {"code": "..."}, valid 10 min, single use
curl -s http://127.0.0.1:7070/admin/pairing-code/<CODE>/status | jq    # used / claimed_by
curl -s http://127.0.0.1:7070/clients | jq                             # enrolled clients + live connected flag
```

### 1.7 Testing failure modes

- **Server crash:** `dev/dev.sh restart-server`. The client logs a connection
  error, retries with backoff of 1s, 2s, 4s, and so on up to 30s, then
  re-authenticates.
- **Silent network loss** (no FIN/RST, like dead Wi-Fi): drop the container's
  packets with `iptables`, or `docker network disconnect` the client. The
  server should drop the client after about 30s without heartbeats. The client
  should give up after about 30s without a `heartbeat_ack` and reconnect.
- **Pairing code reuse or expiry:** enroll a second client with a used code, or
  one older than 10 minutes. Enrollment should be rejected.

### 1.8 Configuration (`config.env`)

```bash
cp config.env.example config.env   # gitignored; every key is optional
```

Each key is resolved in this order: **real environment variable**, then
**config file**, then **built-in default** (the values in the example file).
Both binaries log the file they loaded and the timings in effect at startup,
so check the first lines of the log to confirm a change took effect.

Where the file is read from:

| How it runs              | File                                                                                   |
| ------------------------ | -------------------------------------------------------------------------------------- |
| Any Compose stack        | `config.env` in the repo root, passed in via `env_file` (optional)                      |
| Native server            | `$DELEGATION_CONFIG`, else `./config.env` (relative to the working directory)            |
| Native client            | `$DELEGATION_CONFIG`, else `./config.env`, else `~/.config/delegation-client/config.env` |

| Key                                    | Default              | Used by |
| -------------------------------------- | -------------------- | ------- |
| `SERVER_PORT`                          | `7070`               | server  |
| `SERVER_AUTH_TIMEOUT_MS`               | `5000`               | server  |
| `SERVER_HEARTBEAT_TIMEOUT_MS`          | `30000`              | server  |
| `SERVER_HEARTBEAT_SWEEP_INTERVAL_MS`   | `10000`              | server  |
| `SERVER_PAIRING_CODE_TTL_MS`           | `600000`             | server  |
| `DB_PATH`                              | `data/delegation.db` | server (Docker image sets `/app/data/delegation.db`) |
| `FRONTEND_DIR`                         | `frontend/dist`      | server (Docker image sets `/app/frontend/dist`) |
| `CLIENT_HEARTBEAT_INTERVAL_MS`         | `10000`              | client  |
| `CLIENT_ACK_TIMEOUT_MS`                | `30000`              | client  |
| `CLIENT_RECONNECT_INITIAL_BACKOFF_MS`  | `1000`               | client  |
| `CLIENT_RECONNECT_MAX_BACKOFF_MS`      | `30000`              | client  |

A value that is invalid or `0` logs a warning and falls back to its default.
`SERVER_PORT` changes only the port the server listens on. The dev port
mapping, `dev/dev.sh` and the Cloudflare Tunnel route all assume `7070`, so
update them too if you change it.

To try timing changes quickly in dev, set short values (for example
`SERVER_HEARTBEAT_TIMEOUT_MS=4000`, `CLIENT_HEARTBEAT_INTERVAL_MS=1500`), run
`dev/dev.sh up`, and watch `dev/dev.sh logs`. Compose recreates containers
whose environment changed.

---

## Part 2: Deployment

### 2.0 Hardware assumptions

Nothing is tied to particular hardware, but a few constraints apply:

- **OS:** any Linux distro that runs Docker for the server. Native clients
  must be Linux; the identity file relies on Unix permission bits. Windows and
  macOS hosts can run the client only in Docker.
- **CPU architecture:** only the server image is prebuilt, by the Deploy
  workflow (§2.8), for `linux/amd64` unless you add more platforms. Otherwise
  every image is built
  from source, normally on the machine that runs it. This works the same on
  `x86_64` and `arm64` (for example a Raspberry Pi 4/5) because the base images
  (`rust:1-slim-bookworm`, `debian:bookworm-slim`, `node:20-alpine`) are
  multi-arch. 32-bit ARM (`armv7`) may build but is untested.
- **Memory:** compiling Rust is the heaviest step. On hosts with 2 GB of RAM
  or less, add swap or build elsewhere (see §2.4). Once built, the server and
  client each use only a few MB.
- **Disk:** about 3 GB free for build layers. Running images are under 150 MB.
- **Network:** both sides need outbound HTTPS (443). Neither side needs an
  inbound port.

### 2.1 Prepare the Cloudflare side (one time)

The deployment lives at `llm.smith-c.com`, which is also the client compose
file's default server. For a different domain, substitute it everywhere below.

1. **Create the tunnel:** in Zero Trust, go to **Networks → Tunnels → Create
   a tunnel → Cloudflared** and pick the Docker connector. Copy the **tunnel
   token** from the `docker run` command it shows.
2. **Add a public hostname:** route `llm.smith-c.com` to service
   `http://delegation-server:7070`. That is the Compose service name, which
   resolves on the Compose network.
3. **Put Cloudflare Access in front of the whole site.** The server has **no
   built-in authentication** on `/admin/*`, `/clients`, or the dashboard, so
   Access is what protects them. Under **Access → Applications**:
   - **Application 1 (whole site):** self-hosted, domain `llm.smith-c.com`
     with no path. Add an **Allow** policy for only your identity or identities.
   - **Application 2 (client endpoints):** self-hosted, with two destinations:
     `llm.smith-c.com/ws*` and `llm.smith-c.com/enroll*`. Add a **Bypass**
     policy for Everyone. Clients are headless and can't complete an Access
     login. On these paths they authenticate with single-use pairing codes
     (`/enroll`) and Ed25519 challenge signatures (`/ws`). The more specific
     path application takes precedence over application 1.
   - **Verify** from any machine:

     ```bash
     curl -sI https://llm.smith-c.com/clients | head -1    # 302 to the Access login, not 200
     curl -s -X POST https://llm.smith-c.com/enroll \
       -H 'Content-Type: application/json' \
       -d '{"pairing_code":"X","public_key":"X","hostname":"X"}'
     #   {"error":"invalid or expired pairing code"}  ← reached the server; bypass works
     ```

     A private browser window at `https://llm.smith-c.com/` should show the
     Access login.

### 2.2 Deploy the server

On the server host:

```bash
# 1. Install Docker Engine + Compose plugin (https://docs.docker.com/engine/install/)
sudo usermod -aG docker "$USER"   # log out and back in afterwards

# 2. Get the code
git clone <repo-url> delegation
cd delegation/server

# 3. Provide the tunnel token (.env is gitignored)
echo "TUNNEL_TOKEN=<paste token>" > .env
chmod 600 .env

# 4. Optional: tune timings (see §1.8). Read from the repo root.
cp ../config.env.example ../config.env

# 5. Build and start (the first build takes minutes on small hosts)
docker compose up -d --build

# 6. Verify
docker compose ps
docker compose logs -f delegation-server   # "[config] ..." lines, then "listening on :7070"
docker compose logs cloudflared            # "Registered tunnel connection"
```

Then open `https://llm.smith-c.com/` in a browser. You should pass Access
and see the dashboard with no clients.

Both containers use `restart: unless-stopped`, so they come back after a
reboot as long as the Docker service is enabled (`sudo systemctl enable docker`).

### 2.3 Enroll a client

**Mint a pairing code** with the dashboard's **Add client** button. It shows
the code and updates when the code is claimed. The code is valid for 10
minutes and works once.

Because the admin endpoint sits behind Access, `curl` from outside won't
work. From the server host you can call the server directly inside the
container network. Neither image ships `curl`, so use a throwaway container:

```bash
docker run --rm --network server_default curlimages/curl \
  -fsS -X POST http://delegation-server:7070/admin/pairing-code
```

(The network name is `<compose-project>_default`. The project name defaults
to the directory name, which is `server`. Check with `docker network ls`.)

On each client machine, choose **one** of the following.

#### Option A: Docker client (any OS with Docker, any arch)

```bash
git clone <repo-url> delegation
cd delegation/client
cat > .env <<'EOF'
DELEGATION_SERVER=https://llm.smith-c.com
PAIRING_CODE=ABC12345
EOF
docker compose up -d --build
docker compose logs -f     # expect: enrolled, client_id=... / authenticated / heartbeat acked
```

To tune client timings, put a `config.env` in the repo root (`cp
config.env.example config.env`). Compose passes it to the container.

**Hardware report.** The compose file mounts the host's `/` read-only at
`/hostfs` and its SMBIOS table at `/hostdmi`, so the dashboard shows the
host's disks, free space, OS and RAM type rather than the container's.
This gives code in the container read access to everything on the host,
SSH keys included. It is only the client binary, but if you'd rather not,
delete the `/:/hostfs:ro` volume and the `DELEGATION_HOST_ROOT` variable;
the client then reports the container's view of disks and OS.

GPUs need an override file on top of the base one:

```bash
# NVIDIA (needs the NVIDIA Container Toolkit on the host)
docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d --build
# AMD
docker compose -f docker-compose.yml -f docker-compose.amd.yml up -d --build
```

The identity is stored in the named volume `client-identity`. You can leave
`PAIRING_CODE` in `.env` after enrollment because the client ignores it once
an identity exists. **Don't run `docker compose down -v`**: deleting the
volume deletes the identity, and the client would need a new code.

#### Option B: Native client (Linux, any arch)

```bash
# 1. Toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
sudo apt install -y build-essential git   # or your distro's equivalent (gcc, make)

# 2. Build
git clone <repo-url> delegation
cd delegation
cargo build --release -p delegation-client
sudo install -m 755 target/release/delegation-client /usr/local/bin/

# 3. Enroll once, interactively (writes ~/.config/delegation-client/identity.json, mode 0600)
delegation-client --server https://llm.smith-c.com --pairing-code ABC12345
#   wait for "authenticated", then Ctrl-C

# 4. Optional: client timings (CLIENT_* keys, see §1.8)
cp config.env.example ~/.config/delegation-client/config.env
```

Running as a normal user, the hardware report can't read the SMBIOS table,
so the dashboard shows RAM size but not its type or speed. Everything else
(CPU, GPUs, disks) works without root. `delegation-client --dump-specs`
prints what would be reported.

Then run it as a systemd user service so it survives logouts and reboots:

```bash
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/delegation-client.service <<'EOF'
[Unit]
Description=Delegation client
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/delegation-client --server https://llm.smith-c.com
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now delegation-client
sudo loginctl enable-linger "$USER"     # start at boot without a login session
journalctl --user -u delegation-client -f
```

Back up `~/.config/delegation-client/identity.json` like an SSH private key.
It is the client's whole identity.

#### Option C: Windows tray app

The installer is built by the **Windows** GitHub Actions workflow on every
push to `main` and every PR (or run it by hand from the Actions tab).
Download `DelegationTray-Setup-<version>` from the run's Artifacts.

It installs per user into `%LOCALAPPDATA%\Programs\Delegation` (no admin
prompt), starts at login, and offers to launch at the end. It isn't code
signed, so SmartScreen warns: choose **More info → Run anyway**.

On first launch the "Add server" window opens: enter the server URL, then
the pairing code. The tray menu lists every saved server; the active one
has a green (connected) or red (disconnected) dot. Click it to toggle,
click another server to switch to it, or use **Add server…** for more.

Files:

| Path                                          | What                                          |
| --------------------------------------------- | --------------------------------------------- |
| `%APPDATA%\delegation-client\servers.json`    | Saved servers with their private keys         |
| `%APPDATA%\delegation-client\config.env`      | Optional client timings (see §1.8)            |
| `%LOCALAPPDATA%\delegation-client\tray.log`   | Log (rotated at 1 MB)                         |

Uninstalling removes the app and the login entry but deliberately leaves
`servers.json`, since it holds the keys; delete the folder by hand to
forget all pairings. To remove one saved server, quit the tray and edit
`servers.json`. `delegation-tray.exe --dump-specs` prints the hardware
report.

**Verify:** the client shows up on the dashboard with a green status dot
within a few seconds, and its card's details list its hardware.

### 2.4 Low-memory or slow hosts: build elsewhere

If `cargo build` runs out of memory or takes too long on the target (common
on 1 to 2 GB single-board computers), use either of these.

**Add swap** (simplest):

```bash
sudo fallocate -l 2G /swapfile && sudo chmod 600 /swapfile
sudo mkswap /swapfile && sudo swapon /swapfile
```

**Cross-build the image on the dev machine** and ship it over SSH:

```bash
# on the dev machine, from repo root (one-time: enable QEMU emulation)
docker run --privileged --rm tonistiigi/binfmt --install arm64

docker buildx build --platform linux/arm64 -f server/Dockerfile \
  -t delegation-server:latest --load .
docker save delegation-server:latest | gzip | ssh user@server-host 'gunzip | docker load'
```

Then, on the target, point Compose at the loaded image with a gitignored
override file and skip the build:

```bash
cd delegation/server
cat > docker-compose.override.yml <<'EOF'
services:
  delegation-server:
    image: delegation-server:latest
EOF
docker compose up -d --no-build
```

The same approach works for the client: use `-f client/Dockerfile`, tag it
`delegation-client:latest`, and override the `delegation-client` service.
Under QEMU emulation, expect the cross build to be several times slower
than a native build.

### 2.5 Updating

**Update the server before the clients** whenever the protocol changes.
A new server accepts old clients, but an old server doesn't understand a
new client's heartbeats and never acks them, so the client keeps timing out
and reconnecting.

```bash
# server host (skip this if the Deploy workflow is set up; see §2.8)
cd delegation && git pull
cd server && docker compose up -d --build     # DB persists in the server-data volume

# Docker client
cd delegation && git pull
cd client && docker compose up -d --build     # identity persists in the client-identity volume

# native client
cd delegation && git pull
cargo build --release -p delegation-client
sudo install -m 755 target/release/delegation-client /usr/local/bin/
systemctl --user restart delegation-client
```

During a server restart, connected clients drop and reconnect on their own
using backoff. You don't need to touch them.

Clean up old build layers occasionally with `docker image prune` and
`docker builder prune`.

### 2.6 Backups

The server's state is one SQLite file in the `server-data` volume:

```bash
cd delegation/server
docker compose stop delegation-server
docker run --rm -v server_server-data:/data -v "$PWD":/backup debian:bookworm-slim \
  cp /data/delegation.db /backup/delegation-$(date +%F).db
docker compose start delegation-server
```

If the database is lost, every client has to be re-enrolled with a new
pairing code: wipe its identity file or volume, then enroll again.

### 2.7 Troubleshooting

| Symptom                                           | Likely cause / fix                                                                                             |
| ------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| Dashboard 404s at `/`                             | Frontend wasn't built into the image. Rebuild with `--build`; the log warns `no index.html in ...`             |
| Cloudflare 502 / 1033                             | Tunnel route doesn't point at `http://delegation-server:7070`, or cloudflared isn't running (`docker compose logs cloudflared`) |
| Client enroll returns HTML or a redirect          | Access is intercepting `/enroll`. Add the Bypass application from §2.1                                         |
| Client authenticates then loops reconnecting      | `/ws` is behind Access, or WebSockets are disabled for the zone (Cloudflare → Network → WebSockets)            |
| Enroll rejected                                   | Code is expired (10 min), already used, or mistyped. Mint a new one                                            |
| Client's `client_id` unknown at handshake         | Server DB was wiped or replaced. Delete the client's identity and re-enroll                                    |
| Card says "No hardware info yet"                  | The client predates hardware reporting, or hasn't connected since the server was updated. Update the client    |
| RAM type/speed shows `—`                          | Native Linux client running as a normal user (no SMBIOS access). Expected; run as root or in Docker to get it  |
| New client keeps reconnecting after "authenticated" | The server is older than the client. Update the server first (§2.5)                                          |
| `cargo build` fails with "edition 2024" errors    | Toolchain is too old. Use rustup stable, not the distro `rustc`                                                |
| Build killed with no error on small hosts         | Out of memory. See §2.4                                                                                        |

### 2.8 Automated server deploys (GitHub Actions)

`.github/workflows/deploy.yml` deploys the server once CI passes on `main`:

1. **publish** builds `server/Dockerfile` and pushes it to the private GHCR
   package `ghcr.io/sterance/delegation-server`, tagged with the commit SHA
   and `latest`.
2. **deploy** runs in the `production` environment. It SSHes to the host
   through the Cloudflare Tunnel, authenticating to Access with a service
   token, and runs `server/deploy.sh deploy <sha>`. The job's `GITHUB_TOKEN`
   is sent over stdin so the host can pull the private image. It is used for
   that one pull and never stored on the host.
3. **`deploy.sh`** pulls the image, records it in `server/.env`
   (`DELEGATION_IMAGE`, `DELEGATION_TAG`), recreates the stack without
   building, and probes `/clients` for up to 60 s. If the probe fails, it
   switches back to the previous image and the job fails. It keeps the
   current and previous image and deletes older tags.

The host never opens an inbound port. The SSH key can only run `deploy.sh`.

To redeploy or roll back from GitHub, run the workflow manually (**Actions →
Deploy → Run workflow**). Leave `image_tag` empty to build and deploy the
selected branch, or enter an already-published commit SHA. On the host,
`server/deploy.sh rollback` swaps back to the previous image, and
`server/deploy.sh status` shows what is running.

**What CI does not update:** `deploy.sh` deploys only the image. Changes to
`server/docker-compose.yml`, `deploy.sh` itself or `config.env` still need
`git pull` on the host, followed by `server/deploy.sh deploy <current sha>`
or `docker compose up -d`. This is deliberate. A compose file can mount the
host's filesystem, so letting CI push one would turn the restricted key back
into full host access.

#### Host setup (one time)

Do this on top of §2.2, in the same checkout.

```bash
# 1. A dedicated deploy key, with no passphrase (generate it anywhere, keep the private half for GitHub)
ssh-keygen -t ed25519 -N '' -C delegation-deploy -f deploy_key

# 2. Allow it to run deploy.sh and nothing else. Add one line to
#    ~/.ssh/authorized_keys of the user that owns the checkout and is in the
#    docker group (adjust the path):
command="/home/<user>/delegation/server/deploy.sh",restrict ssh-ed25519 AAAA... delegation-deploy

# 3. The host key that CI will pin (the part after the hostname is DEPLOY_HOST_KEY)
cat /etc/ssh/ssh_host_ed25519_key.pub

# 4. Pick up the compose change that lets cloudflared reach sshd
cd ~/delegation && git pull && cd server && docker compose up -d
```

`restrict` turns off port forwarding, agent forwarding and PTY allocation for
that key. Your own login keys are unaffected.

sshd must accept connections from the Docker bridge, because cloudflared
reaches it at `host.docker.internal:22`. The default `ListenAddress` does. If
a firewall such as ufw is enabled, allow port 22 from the Docker subnets
(for example `sudo ufw allow from 172.16.0.0/12 to any port 22`). sshd
doesn't have to be reachable from the internet or the LAN.

To run deploy.sh as a separate `deploy` user instead, add that user to the
`docker` group and give it write access to `server/` (deploy.sh writes
`.env`, `.deploy-previous` and `.deploy.lock`). Membership in the `docker`
group is root-equivalent either way. The forced command is the real limit.

#### Cloudflare setup (one time)

1. In the tunnel from §2.1, add a public hostname such as
   `ssh.smith-c.com`, with service type **SSH** and URL
   `host.docker.internal:22`.
2. **Access → Service Auth → Service Tokens:** create a token (for example
   `github-deploy`). Copy the Client ID and Client Secret.
3. **Access → Applications:** add a self-hosted application for
   `ssh.smith-c.com` with a single **Service Auth** policy that includes only
   that token. If your whole-site application from §2.1 uses a wildcard, make
   sure it doesn't also cover this hostname.

#### GitHub setup (one time)

In **Settings → Environments**, create `production`. Under **Deployment
branches and tags**, choose **Selected branches** and add `main`. Optionally
add yourself as a required reviewer. Then add these to the environment, not
to the repository:

| Kind     | Name                      | Value                                                   |
| -------- | ------------------------- | ------------------------------------------------------- |
| Secret   | `DEPLOY_SSH_KEY`          | Contents of the private `deploy_key`                     |
| Secret   | `CF_ACCESS_CLIENT_ID`     | Service token Client ID                                  |
| Secret   | `CF_ACCESS_CLIENT_SECRET` | Service token Client Secret                              |
| Variable | `DEPLOY_HOST`             | `ssh.smith-c.com`                                        |
| Variable | `DEPLOY_USER`             | The user whose `authorized_keys` has the deploy key      |
| Variable | `DEPLOY_HOST_KEY`         | `ssh-ed25519 AAAA...` from step 3 (no hostname, no comment) |

Optional repository variable: `DEPLOY_PLATFORMS`, for example
`linux/amd64,linux/arm64` for an ARM host. Non-native platforms build under
QEMU and take much longer.

The first publish creates the package as private and links it to this repo,
so this repo's workflows can pull it with `GITHUB_TOKEN`. Check
**Package settings → Manage Actions access** if a pull is ever denied.

**Test the path from a dev machine** before the first run (needs
`cloudflared` locally):

```bash
ssh -i deploy_key \
  -o ProxyCommand="cloudflared access ssh --hostname %h --service-token-id <ID> --service-token-secret <SECRET>" \
  <user>@ssh.smith-c.com status
```

It should print the deployed image and `docker compose ps` output, and
nothing else. An interactive shell must not be possible with this key.
