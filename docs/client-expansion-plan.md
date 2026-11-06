# Implementation plan: client refactor, hardware specs, Windows tray

This covers three phases:

- **Phase 0:** restructure `client/` into a library with two front ends.
- **Phase 1:** collect hardware specs from clients and show them in the dashboard's client card.
- **Phase 2:** the native Windows tray client from `delegation-windows-client-spec.md`, with an installer built by GitHub Actions.

Decisions already made:

- Protocol changes are allowed when they make the design cleaner. This overrides the spec's "don't change `protocol/`" rule.
- Disk "speed" means media type and interface (NVMe/SATA/USB, SSD/HDD).
- On Linux, RAM type and speed may show as unknown when the client isn't running as root.
- The Docker client must report the host's real hardware.
- The Windows build and installer are produced by GitHub Actions on every run, for testing on real Windows hardware.

**Prerequisite:** commit the current uncommitted changes first, so this work starts from a clean base.

---

## Phase 0: Restructure the client (no behaviour change)

### 0.1 Turn `client/` into a library plus binaries

```
client/src/
  lib.rs            pub mod identity, session, hardware, log, paths
  identity.rs       Identity, enroll(), single-file store (used by the CLI)
  session.rs        run_session + reconnect loop, controlled through channels
  hardware/         (empty stub until Phase 1)
  bin/delegation-client.rs   the CLI (today's main.rs, made thinner)
  bin/delegation-tray/       Phase 2; only built on Windows
```

- The binary name `delegation-client` stays the same, so the Dockerfile and entrypoint don't change.
- Tray-only dependencies go under `[target.'cfg(windows)'.dependencies]`. Linux builds and the Docker image never compile them.

### 0.2 Session API

```rust
pub enum SessionCmd { Connect(ServerIdentity), Disconnect, Shutdown }
pub enum SessionStatus { Connecting, Connected, Disconnected { error: Option<String> }, UnknownClient }
pub async fn controller(cmds: mpsc::Receiver<SessionCmd>, status: impl Fn(SessionStatus), timings: Timings, info: ClientInfo)
```

- The controller runs at most one session task at a time. `Connect` aborts whichever session is running and starts a new one, which covers the spec's "switching tears down" rule. `Disconnect` aborts it, and the server sees the socket close.
- Today `handle_unknown_client()` calls `process::exit`. It will instead return `SessionError::UnknownClient`, after which the controller stops retrying and reports `UnknownClient`.
- The CLI handles `UnknownClient` the way it does now: clear `identity.json`, print the hint, exit 1.

### 0.3 Small fixes needed for portability

- **Paths (`paths.rs`):** `config_dir()` keeps today's `$HOME/.config/delegation-client` on Unix, so the Docker volume path doesn't change. On Windows it uses `%APPDATA%\delegation-client` via the `dirs` crate.
- **Identity file permissions:** wrap the `0o600` call in `#[cfg(unix)]`. Files under `%APPDATA%` are already private to the user on Windows.
- **Hostname:** replace both `Command::new("hostname")` calls with the `gethostname` crate. The Dockerfile then no longer needs to install `hostname`.
- **Logging:** add a ~20-line logger (stdout for the CLI, a file for the tray) that keeps the current `[ts] [tag] msg` format, using the `log` crate macros. Without this, `println!` output from the tray would simply be lost.
- **Version pins:** remove the Rust 1.75-era pins (`=0.20.1`, `=2.0.0`, `url`, `idna`, …) and run `cargo update`. `tray-icon`, `eframe` and `wmi` need a current toolchain anyway.

**Check:** the workspace builds, and the Docker client still enrolls, heartbeats and self-heals against `dev/`.

---

## Phase 1: Hardware specs on Linux, plus protocol, server and dashboard

### 1.1 Protocol changes (`protocol/src/lib.rs`)

- **Remove `hostname` from `Heartbeat`.** Heartbeats become just `{ timestamp }`.
- **Add a new message sent once after `AuthOk` on every connection:**
  ```rust
  ClientMessage::Info { hostname, os, client_version, specs: HardwareSpecs }
  ```
  The hostname then updates in the dashboard if a machine is renamed. At the moment the dashboard keeps the name the machine had when it enrolled.
- **Make `AuthFailed.reason` a typed enum** with serde renames (`"unknown client_id"`, `"signature verification failed"`), so it sends exactly the same strings as today. This replaces the string constant the client currently compares against.
- **Spec types:** everything is optional and in raw units. The dashboard does the formatting.
  ```rust
  HardwareSpecs { cpu: Option<Cpu>, memory: Option<Memory>, gpus: Vec<Gpu>, disks: Vec<Disk> }
  Cpu    { model, max_mhz, physical_cores, logical_cores }
  Memory { total_bytes, standard /* "DDR5" */, speed_mts, module_count }
  Gpu    { model, vendor, vram_bytes, vram_shared: bool, max_core_mhz }
  Disk   { model, media: Hdd|Ssd|Unknown, interface: Nvme|Sata|Sas|Usb|Virtual|Other, total_bytes, available_bytes }
  ```
- **Deploy order:** server first, then clients. The new server accepts old heartbeats, because serde ignores the extra `hostname` field. An old server would reject the new heartbeat, never ack it, and the new client would keep timing out and reconnecting.

### 1.2 Hardware collection: shared code (`hardware/mod.rs`)

- `pub fn collect() -> HardwareSpecs` runs once at process start, on a blocking thread because it is slow. The result is cached, then sent after each successful auth.
- Each collector fails on its own and leaves its field as `None`, and the whole collection is time-limited, so it can never block connecting.
- **CPU:** model and physical/logical core counts come from `sysinfo` on both OSes.
- **RAM:** total comes from `sysinfo` on both OSes.
- **SMBIOS parser (`smbios.rs`):** about 80 lines, hand-written to keep dependencies light. It reads type 17 (Memory Device) records for type code → DDR3/4/5/LPDDR\*, speed or configured speed, and module count. **Linux and Windows both use it**, so the type-code mapping only exists once and is unit-tested once.
- **NVIDIA GPUs:** `nvml-wrapper` loads `libnvidia-ml.so.1` or `nvml.dll` at runtime and gives model, VRAM and max graphics clock. On machines without NVIDIA it does nothing.

### 1.3 Linux collectors (`hardware/linux.rs`)

Every path read here goes through an optional `DELEGATION_HOST_ROOT` prefix so the same code works in Docker (see 1.4).

| Field              | Source                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| ------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| CPU max MHz        | `/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq`, falling back to `/proc/cpuinfo`                                                                                                                                                                                                                                                                                                                                                                                     |
| RAM type and speed | `/sys/firmware/dmi/tables/DMI` → SMBIOS parser. Only readable as root; otherwise these show as unknown.                                                                                                                                                                                                                                                                                                                                                                      |
| AMD and Intel GPUs | `/sys/class/drm/card*/device`: vendor ID, `mem_info_vram_total`, top entry in `pp_dpm_sclk`. Intel integrated GPUs report `vram_shared = true`. Model name via `lspci -vmm -s <slot>`, falling back to `"AMD [1002:744c]"`. Cards NVML already reported are skipped (matched by PCI bus ID).                                                                                                                                                                                 |
| Disks              | One entry per physical disk in `/sys/block/*`, skipping `loop`, `ram`, `zram`, `dm-*` and `md*`. Model from `device/model`; size from `size × 512`. Media: `queue/rotational` gives HDD or SSD. Interface: the resolved sysfs path contains `nvme`, `/ata`, `/usb` or `virtio`. Free space: for each mount in `mountinfo` on a real filesystem, take the `major:minor` → parent disk via `/sys/dev/block`, add up `statvfs` free space, and count each filesystem only once. |

Unit tests use fixture files (a sample `cpuinfo`, a sysfs tree, captured SMBIOS bytes).

### 1.4 Docker client reports the host's hardware

Changes to `client/docker-compose.yml`:

```yaml
volumes:
  - client-identity:/root/.config/delegation-client
  - /:/hostfs:ro # host mounts, so disk free space is the host's
  - /sys/firmware/dmi/tables:/hostdmi:ro # Docker hides /sys/firmware; mounting it elsewhere works
environment:
  DELEGATION_HOST_ROOT: /hostfs
  DELEGATION_DMI_PATH: /hostdmi/DMI
```

- **CPU and RAM total:** `/proc` and `/sys` inside the container already show the host's values.
- **RAM type:** the container runs as root, so with the DMI mount above it gets RAM type and speed, which a native client running as a normal user does not.
- **Disks:** read the host's mount table from `/hostfs/proc/1/mountinfo` and run `statvfs` on each mount under `/hostfs`. This needs checking on real hardware. If it doesn't work, fall back to walking the distinct devices under `/hostfs`.
- **Security trade-off:** mounting the host's whole filesystem read-only exposes all of it, SSH keys included, to code in the container. It is our own code, but the README should say so.
- **NVIDIA GPU:** a separate `docker-compose.gpu.yml` override adds `deploy.resources.reservations.devices: [{driver: nvidia, capabilities: [gpu, utility]}]`. `utility` is what makes NVML available inside the container. It has to be a separate file because the base compose file must still start on machines without the NVIDIA container toolkit.
- **AMD GPU:** `docker-compose.amd.yml` passes through `/dev/dri` and `/dev/kfd`.
- **Dockerfile:** install `pciutils` (for `lspci`) instead of `hostname`.

### 1.5 Server

- **`db.rs` migration:** after the `CREATE TABLE`s, check `PRAGMA table_info(clients)` and run `ALTER TABLE ADD COLUMN` for any missing column: `os`, `client_version`, `specs_json`, `info_updated_at`. This is safe to run on the live database repeatedly.
- **WebSocket handler:** on `ClientMessage::Info`, update the hostname, os, version and specs (stored as JSON) in the database. It is not acked.
- **`/clients`:** the response gains `os`, `client_version`, `specs: Option<HardwareSpecs>` (parsed back into the typed struct) and `info_updated_at`.

### 1.6 Frontend

- **`api.ts`:** add TypeScript types mirroring `HardwareSpecs`, written by hand with a comment pointing at `protocol/src/lib.rs`. Not worth adding `ts-rs` for five structs.
- **`ClientCard.tsx` header:** a one-line summary under the hostname (e.g. `RTX 4090 · 64 GB DDR5`).
- **Expanded section:** a "Hardware" block built from the existing `Timestamp`-style label/value rows:
  - **CPU:** `Ryzen 9 7950X · 5.7 GHz · 16C/32T`
  - **RAM:** `64 GB DDR5-6000 (2 modules)`
  - **GPUs:** one row each, `RTX 4090 · 24 GB · 2.52 GHz`; integrated GPUs show `shared memory`
  - **Disks:** one row each, `Samsung 990 Pro · NVMe SSD · 1.2 TB free of 2 TB`
  - OS and client version go at the bottom.
- **Formatting helpers:** a small `formatBytes` / `formatMhz`. Missing values show `—`; a client that has never sent specs shows "No hardware info yet".

**Check:** the dev environment and lappy natively (as a normal user and as root), the Docker client with the host mounts, and an old client still connecting to the new server.

---

## Phase 2: Windows tray client and installer

### 2.1 Windows hardware collectors (`hardware/windows.rs`)

| Field              | Source                                                                                                                                                                                                                                                                          |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| CPU max MHz        | Registry `HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0\~MHz` (no WMI needed)                                                                                                                                                                                             |
| RAM type and speed | `GetSystemFirmwareTable('RSMB')` → the same SMBIOS parser. No admin rights needed.                                                                                                                                                                                              |
| GPUs               | DXGI `EnumAdapters1`: name, `DedicatedVideoMemory` (64-bit), vendor; skip the "Microsoft Basic Render Driver". NVIDIA clocks come from NVML. **Don't use** `Win32_VideoController.AdapterRAM`: it is 32-bit and tops out at 4 GB.                                               |
| Disks              | WMI (`wmi` crate) `root\Microsoft\Windows\Storage`: `MSFT_PhysicalDisk` gives model, `MediaType` (3 = HDD, 4 = SSD) and `BusType` (17 = NVMe, 11 = SATA, 7 = USB…). `MSFT_Partition` gives disk number and drive letter, and `GetDiskFreeSpaceExW` gives free space per letter. |

### 2.2 Tray app (`bin/delegation-tray/`)

- **`main.rs`:**
  - `#![windows_subsystem = "windows"]` so no console window appears.
  - A **single-instance named mutex** (`Local\DelegationTray`). Startup at login and the installer's "launch after install" could otherwise start two copies. The installer also uses this mutex to close the running app before upgrading.
  - Starts the logger, which writes to `%LOCALAPPDATA%\delegation-client\tray.log`.
  - Collects hardware specs on a background thread.
  - Starts the tokio runtime and session controller on their own thread, and runs the `winit` event loop on the main thread.
- **`store.rs`:** `servers.json` in `%APPDATA%\delegation-client\`:
  ```json
  { "active": 0, "manually_disconnected": false, "servers": [{ "url": "...", "client_id": "...", "signing_key_b64": "..." }] }
  ```

  - Saved by writing a temp file and renaming it over the old one, so a crash can't leave it half-written.
  - URLs are normalized (add `https://` if no scheme, lowercase the host, drop a trailing `/`), and that normalized form is what the "do we already know this server?" check compares.
  - The "Server forgot this client" self-heal clears only that server's `client_id` and key.
- **`menu.rs`:**
  - Built with `muda`: one `IconMenuItem` per server, then a separator, `Add server…` and `Quit`.
  - The status dot icons are drawn in code as RGBA (green, red, blank), so there are no image files to manage. Only the active server's row shows a dot.
  - On a status change it calls `set_icon` on that row instead of rebuilding the menu. The menu is rebuilt only when a server is added.
  - Menu events and session status messages both reach the event loop through an `EventLoopProxy`.
- **Event handling:**

  | Event                          | Action                                                                                                                                                       |
  | ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
  | Click the active server        | Toggle connect/disconnect and save `manually_disconnected`                                                                                                   |
  | Click a saved, inactive server | Make it active, clear `manually_disconnected`, send `Connect`                                                                                                |
  | `UnknownClient` status         | Clear that server's identity and open the form with its URL filled in and a "this server no longer recognizes this machine" note                             |
  | `Quit`                         | Send `Shutdown`, wait briefly for the connection to close cleanly, exit                                                                                      |
  | App start                      | Load the store. If there's an active server, it has an identity and it wasn't manually disconnected, send `Connect`. If no servers are saved, open the form. |

- **`form.rs`:** the same exe started as `--add-server-form [--url X] [--note Y]`. It uses `eframe` and runs as a separate process, so it never clashes with the tray's event loop.
  - It reads `servers.json` read-only for the "already known" check. If the URL is known, it prints `{"existing": url}` and exits.
  - Otherwise it shows the pairing-code field and calls `identity::enroll()` itself. Errors (bad URL, invalid or expired code, server unreachable) appear in the form, which stays open for another try.
  - On success it prints `{"enrolled": {url, client_id, signing_key_b64}}`.
  - The tray reads that output on a helper thread, saves the new server, makes it active and connects. Only one form can be open at a time.
- **Starting at login:** **the installer handles this**, not the app (see 2.3), so uninstalling removes it cleanly. This replaces the spec's suggestion that the app add itself to startup.
- **Exe icon:** `embed-resource` in `build.rs` embeds a committed `client/assets/delegation.ico`, which the installer also uses.

### 2.3 GitHub Actions (the repo is `sterance/delegation`)

- **`.github/workflows/ci.yml`** runs on ubuntu-latest for pushes and PRs:
  - `cargo fmt --check`, `clippy` and `test` for the whole workspace
  - `docker build` for the client and server images
  - `npm ci && npm run build` for the frontend
- **`.github/workflows/windows.yml`** runs on pushes to main, PRs and manual runs, on `windows-latest`:
  1. `dtolnay/rust-toolchain@stable` and `Swatinem/rust-cache`
  2. `cargo test -p delegation-client`
  3. Run `delegation-tray --dump-specs` and print the JSON. This is a quick check of the Windows collectors on the runner's virtual hardware, and the flag is handy for debugging on real machines too.
  4. `cargo build --release -p delegation-client --bin delegation-tray`
  5. Install Inno Setup if the runner image doesn't have it (`choco install innosetup`), then `iscc installer\delegation.iss /DAppVersion=0.1.0.${{ github.run_number }}`
  6. `actions/upload-artifact`: `DelegationTray-Setup-<version>.exe`
- **`installer/delegation.iss`:**
  - `PrivilegesRequired=lowest`, so it installs for the current user only into `{localappdata}\Programs\Delegation` without asking for admin.
  - Adds the `HKCU\…\Run` startup entry, which the uninstaller removes.
  - `AppMutex=DelegationTray`, so the installer closes the running tray before upgrading.
  - A "Launch now" checkbox at the end.
  - It deliberately **leaves `%APPDATA%\delegation-client` behind on uninstall**, because that folder holds the private keys.
- **Not signed:** Windows SmartScreen will warn ("More info → Run anyway"). Code signing stays out of scope.

### 2.4 Testing on a real Windows machine

Go through the spec's checklist using the installer downloaded from the workflow run. On top of that:

- The hardware block in the dashboard is correct, especially VRAM above 4 GB and NVMe vs SATA detection.
- Upgrading by installing a newer build over an older one keeps the saved servers.
- Uninstalling removes the startup entry.

---

## Suggested commit order

1. Phase 0: restructure only. Check that Docker still works.
2. 1.1 + 1.5: protocol and server. **Deploy the server to `llm.smith-c.com` before any new client connects to it.**
3. 1.2–1.4: Linux collectors and Docker mounts.
4. 1.6: dashboard.
5. 2.3 CI workflows first, with only the Linux job and an empty Windows binary, so the Windows pipeline is proven early.
6. 2.1, 2.2, then the installer.

## Risks to check early

- Whether reading the host's mount table from inside Docker works (1.4)
- NVML inside the container
- `eframe` and `tray-icon` versions compiling together on the current toolchain once the pins are removed
