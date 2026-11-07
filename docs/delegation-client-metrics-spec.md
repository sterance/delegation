# Client telemetry overhaul: 1 s tick, metrics and live usage graphs — instructions for Claude Code

## Context

This is the delegation project: Rust/axum server (`server/`), Rust client
(`client/`), shared protocol crate (`protocol/`), React/MUI dashboard
(`frontend/`), and a planned Windows tray front end for the client (see
`delegation-windows-client-spec.md`). Read `guide.md` and `dev/local-test.md`
first.

**This is an overhaul. Every restriction in earlier specs is lifted for this
work.** You may change `protocol/`, the server's WebSocket and database code,
the client's connection loop, and the dashboard freely.

**The project is greenfield, with no live data. Do not preserve backwards
compatibility.** No protocol versioning, no shims for old clients, no
migration paths. Delete what this spec makes redundant rather than keeping it
alongside. If anything here conflicts with heartbeat details in other specs
(including the Windows tray spec), this spec wins.

## Summary of changes

1. The separate heartbeat and heartbeat ack are **replaced by a single `Tick`
   message**, sent by the client every second, carrying the latest resource
   metrics. Ticks are not acknowledged.
2. Liveness is based on **the incoming stream**: the server treats any inbound
   frame as proof of life and drops a connection after several missed ticks.
   The server sends WebSocket pings so the client can detect a dead server.
3. Metrics are collected by a **sampler thread** decoupled from the tick, so a
   slow GPU query can never delay a tick or cause a false disconnect.
4. `last_seen_at` is kept in memory and **flushed to SQLite every 30 s** (and
   on disconnect and shutdown), not written per tick.
5. The server keeps a short **in-memory history** of samples per client.
6. The dashboard **polls every second** and shows each client's history as
   **classic green usage graphs** in the client card's collapsed state.
7. **Every timing, limit and graph setting is tunable in `config.env`** at the
   repo root.

## Configuration: `config.env`

`config.env` already exists at the repo root (with `# SERVER` and `# CLIENT`
headings and `SERVER_PORT`), alongside `config.env.example`. Confirm how the
server container loads it today (`env_file` in compose or similar) and extend
that mechanism; wire it into `server/docker-compose.yml` and
`dev/docker-compose.yml` if it isn't already.

Rules for every variable below:
- Optional, with the listed default compiled in.
- Parsed and validated **once at server startup**, including the
  cross-variable constraints. On an invalid value, fail fast with a message
  naming the variable and the rule it broke.
- Log the effective configuration at startup (info level).
- Document every variable in `config.env.example` with its default and a
  one-line comment, grouped under the headings below. Add them to
  `config.env` too, set to their defaults, so they're visible for tuning.
- Note in `guide.md` that the deployed `config.env` on ANTON is a separate
  copy and needs the new variables (or will run on defaults).

**Client settings are pushed by the server.** Clients run on other machines
(the Windows tray client has no `config.env`), so the client-behaviour
variables are read by the server and sent to each client in the `Welcome`
message at connect time. Changing them means editing `config.env` and
restarting the server; clients pick them up when they reconnect. The client
persists the last settings it received (next to its saved-server entry) so
reconnect backoff works before it has connected; before the first connection
it uses compiled defaults.

### Tick and liveness

| Variable | Default | Where it applies | Meaning |
|---|---|---|---|
| `TICK_INTERVAL_MS` | `1000` | Pushed to client | How often the client sends a `Tick` |
| `SAMPLER_INTERVAL_MS` | `500` | Pushed to client | How often the client's sampler takes a reading. Minimum `200` (CPU % needs two readings at least ~200 ms apart). |
| `SAMPLE_MAX_AGE_MS` | `3000` | Pushed to client | If the newest sample is older than this when a tick is sent, the tick carries no metrics (the sampler is stalled). Must be ≥ 2 × `SAMPLER_INTERVAL_MS`. |
| `LIVENESS_TIMEOUT_MS` | `4000` | Server | If no frame arrives from a client for this long, the server drops the connection. Must be ≥ 2 × `TICK_INTERVAL_MS`. |
| `WS_PING_INTERVAL_MS` | `2000` | Server | How often the server sends a WebSocket ping to each client |
| `SERVER_SILENCE_TIMEOUT_MS` | `6000` | Pushed to client | If nothing arrives from the server for this long, the client reconnects. Must be ≥ 2 × `WS_PING_INTERVAL_MS`. |
| `RECONNECT_BACKOFF_INITIAL_MS` | `1000` | Pushed to client (cached) | First reconnect delay |
| `RECONNECT_BACKOFF_MAX_MS` | `30000` | Pushed to client (cached) | Reconnect delay cap. Must be ≥ initial. |
| `LAST_SEEN_FLUSH_INTERVAL_MS` | `30000` | Server | How often in-memory `last_seen_at` values are written to SQLite |

### Metrics

| Variable | Default | Where it applies | Meaning |
|---|---|---|---|
| `METRICS_HISTORY_SAMPLES` | `60` | Server and dashboard | Samples kept per client in memory, which is also the width of each graph in samples |
| `METRICS_MIN_SPACING_MS` | `500` | Server | Samples arriving from one client closer together than this are dropped (protects against a buggy or hostile client). Must be less than `TICK_INTERVAL_MS`, or every sample would be dropped. |
| `METRICS_MAX_GPUS` | `16` | Server | Samples listing more GPUs than this are rejected |
| `METRICS_MAX_STRING_LEN` | `128` | Server | Max length of string fields such as GPU names |

### Pairing (existing behaviour, now tunable)

| Variable | Default | Where it applies | Meaning |
|---|---|---|---|
| `PAIRING_CODE_TTL_SECS` | `600` | Server | Pairing code validity. Check where this is hardcoded today and replace it. The Add Client modal's "valid for 10 minutes" text must use this value. |
| `DASHBOARD_PAIRING_POLL_INTERVAL_MS` | `1500` | Dashboard | How often the Add Client modal polls the code's status |

### Dashboard

| Variable | Default | Meaning |
|---|---|---|
| `DASHBOARD_POLL_INTERVAL_MS` | `1000` | How often the dashboard polls `GET /clients` |
| `DASHBOARD_PAUSE_WHEN_HIDDEN` | `true` | Stop polling while the browser tab is hidden; poll immediately when it becomes visible |
| `DASHBOARD_GRAPH_WIDTH_PX` | `120` | Width of each usage graph |
| `DASHBOARD_GRAPH_HEIGHT_PX` | `40` | Height of each usage graph |
| `DASHBOARD_GRAPH_GRID_PX` | `10` | Grid cell size |
| `DASHBOARD_GRAPH_LINE_COLOR` | `#00ff00` | Usage line colour |
| `DASHBOARD_GRAPH_GRID_COLOR` | `#008040` | Grid line colour |
| `DASHBOARD_GRAPH_BG_COLOR` | `#000000` | Graph background |
| `DASHBOARD_COLLAPSED_MAX_GPUS` | `1` | How many GPUs get graphs in a card's collapsed state; the rest appear only when expanded |

Validate colours as hex (`#rgb` or `#rrggbb`) and pixel values as positive
integers. The dashboard reads these at runtime (see API below), so changing
them needs a server restart, not a frontend rebuild.

## Protocol (`protocol/`)

- **Remove** the existing heartbeat and heartbeat-ack messages entirely.
- **`Welcome`**: sent by the server immediately after successful
  authentication, carrying a `ClientSettings` struct with the pushed values:
  `tick_interval_ms`, `sampler_interval_ms`, `sample_max_age_ms`,
  `server_silence_timeout_ms`, `reconnect_backoff_initial_ms`,
  `reconnect_backoff_max_ms`. If there is already an auth-success message,
  fold this into it rather than adding a second one. The client sends no ticks
  until it has received `Welcome`.
- **`Tick`**: client to server, every `tick_interval_ms`:

```rust
pub struct Tick {
    pub seq: u64,                  // per-connection counter from 0; for debugging and spotting drops
    pub metrics: Option<Metrics>,  // None if no fresh sample (see SAMPLE_MAX_AGE_MS)
}

pub struct Metrics {
    pub sample_age_ms: u32,        // age of this sample when the tick was sent; avoids relying on client clocks
    pub cpu_percent: f32,
    pub ram_total_bytes: u64,
    pub ram_available_bytes: u64,
    pub swap_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub gpus: Vec<GpuMetrics>,     // empty when no supported GPU
}

pub struct GpuMetrics {
    pub name: String,
    pub utilisation_percent: f32,
    pub vram_used_bytes: u64,
    pub vram_total_bytes: u64,
    pub power_watts: Option<f32>,  // None if the driver reports it as unavailable
}
```

- **No acknowledgement** of ticks.
- The server sends **WebSocket ping frames** every `WS_PING_INTERVAL_MS`.
  Confirm that the client's WebSocket library answers pongs automatically and
  that the client's read loop keeps reading frames so the pongs actually go
  out.
- Keep the struct easy to extend: system power will later be added as
  optional fields, so don't design anything that blocks that.

## Client

### Sampler (metrics collection)

- Runs on **its own thread**, never inside the connection or tick loop. It
  owns a long-lived `sysinfo` `System` and the NVML handle, takes a reading
  every `sampler_interval_ms`, and publishes the latest snapshot with its
  capture time to shared state (a mutex-guarded slot or watch channel).
- Before the first connection, use the compiled default interval. Apply a new
  `sampler_interval_ms` from `Welcome` live, without restarting the thread.
- **CPU and RAM**: `sysinfo`. Report **available** memory, not "free". On
  Linux, "free" excludes reclaimable cache and is misleading. Check what the
  pinned `sysinfo` version's `used_memory()` and `available_memory()` actually
  return rather than assuming. Don't publish a snapshot until CPU % has had
  its two readings.
- **GPU**: `nvml-wrapper`, loading NVML **dynamically**. If the library or
  driver is missing, or initialisation fails, log once at info level and
  report an empty `gpus` list. Treat an "unsupported" error from a single
  query (power especially) as `None`, not a failure. The client must run
  normally on machines without an NVIDIA GPU.
- The sampler lives in the shared client library module (the one the Windows
  tray spec asks for), so the CLI and tray both get it.

### Tick loop

- Timer at `tick_interval_ms` that doesn't drift and **skips missed ticks
  after a stall** rather than sending a burst of backlogged ticks.
- On each tick, read the latest snapshot. If there is none, or it is older
  than `sample_max_age_ms`, send `metrics: None`. Otherwise send it with
  `sample_age_ms` filled in. **Always send the tick**, with or without metrics.

### Server-silence detection and reconnect

- Track the time of the last frame received from the server (pings included).
  If it exceeds `server_silence_timeout_ms`, close the connection and
  reconnect.
- Reconnect with exponential backoff from `reconnect_backoff_initial_ms` up to
  `reconnect_backoff_max_ms`, using the cached settings.

### Docker (Linux client)

- The container sees host-wide CPU and RAM, which is what we want. If a memory
  limit is set on the container, note the difference in a code comment; don't
  add cgroup handling.
- GPU metrics need the NVIDIA Container Toolkit and the `utility` driver
  capability. Don't make that the default in `client/docker-compose.yml`, since
  it would break machines without a GPU. Add an override file such as
  `docker-compose.gpu.yml` and document it in `guide.md`.

## Server

### Liveness

- Per connection, record the time of the last inbound frame of any kind
  (tick, pong, anything else).
- A watchdog checks connections at least every `TICK_INTERVAL_MS`. If one has
  had no inbound frame for `LIVENESS_TIMEOUT_MS`: close it, mark the client
  disconnected, clear its sample history, and flush its `last_seen_at`
  immediately.
- A tick whose metrics fail validation still counts as proof of life.

### `last_seen_at`

- Update it **in memory** on every inbound frame, and track which clients have
  changed since the last flush.
- Every `LAST_SEEN_FLUSH_INTERVAL_MS`, write only the changed rows to SQLite in
  one transaction.
- Also flush on disconnect and on **graceful shutdown** (`docker stop` sends
  SIGTERM; use axum's graceful shutdown so the final flush runs).
- `GET /clients` returns the in-memory value where there is one, so the
  dashboard is always current even between flushes.

### Metrics validation and storage

- Treat metrics as **untrusted client input**. Reject a sample if any of the
  following holds:
  - a percentage is outside 0–100, or any number is NaN or infinite;
  - `gpus` is longer than `METRICS_MAX_GPUS`, or a string is longer than
    `METRICS_MAX_STRING_LEN`;
  - a used value exceeds its total;
  - it arrives less than `METRICS_MIN_SPACING_MS` after the previous one from
    that client.
- A bad sample is dropped and logged at debug level. It must never drop the
  connection or panic.
- Each client's accepted samples go into an **in-memory ring buffer** of
  `METRICS_HISTORY_SAMPLES` entries, stamped with the server's receipt time
  and a **server-wide sequence number** (one counter across all clients,
  incrementing per accepted sample).
- Generate a random **epoch** string at server startup, so the dashboard can
  tell when the server restarted and its sequence numbers reset.
- Nothing metrics-related goes into SQLite, and there are no new tables.
- Don't log per tick at info level. Debug only.

## API

### `GET /clients?since=<seq>`

This replaces the current array response with an object:

```json
{
  "epoch": "3f9c…",
  "latest_seq": 48213,
  "clients": [
    {
      "client_id": "7bc7ce00-…",
      "hostname": "lappy",
      "enrolled_at": "2026-10-06T23:37:15Z",
      "last_seen_at": "2026-11-06T12:01:09Z",
      "connected": true,
      "samples": [
        {
          "seq": 48211,
          "received_at": "2026-11-06T12:01:08.412Z",
          "cpu_percent": 37.2,
          "ram_total_bytes": 33554432000,
          "ram_available_bytes": 21474836480,
          "swap_total_bytes": 8589934592,
          "swap_used_bytes": 0,
          "gpus": [
            { "name": "NVIDIA GeForce RTX 3070", "utilisation_percent": 63.0,
              "vram_used_bytes": 6549825536, "vram_total_bytes": 8589934592,
              "power_watts": 142.5 }
          ]
        }
      ]
    }
  ]
}
```

- `samples` contains only samples with `seq > since`, oldest first, at most
  `METRICS_HISTORY_SAMPLES`. With `since` missing or `0`, it returns each
  client's full buffer, so graphs fill immediately on page load.
- A disconnected client returns an empty `samples` array.
- Update `frontend/src/api.ts` types to match.

### `GET /admin/dashboard-config`

Returns the dashboard settings: all `DASHBOARD_*` values, plus
`METRICS_HISTORY_SAMPLES`, `TICK_INTERVAL_MS` and `PAIRING_CODE_TTL_SECS`. It
sits under `/admin`, so it's already behind Cloudflare Access and already
covered by the Vite dev proxy.

## Dashboard

### Polling

- On load, fetch `/admin/dashboard-config` once. If that fails, log to the
  console and use built-in defaults matching the table above.
- Poll `GET /clients?since=<latest_seq>` every `DASHBOARD_POLL_INTERVAL_MS`.
  Schedule each poll **after the previous one finishes** (chained
  `setTimeout`, not `setInterval`), so slow responses never stack up.
- Keep a per-client sample buffer in component state. Append new samples and
  trim to `METRICS_HISTORY_SAMPLES`. Clear a client's buffer when it shows as
  disconnected.
- If `epoch` changes, or `latest_seq` is lower than the stored value, the
  server has restarted: clear all buffers and re-poll with `since=0`.
- If `DASHBOARD_PAUSE_WHEN_HIDDEN` is set, stop polling while
  `document.visibilityState` is `hidden`, and poll immediately when the tab
  becomes visible again. The `since` cursor fills the gap from the server's
  history.
- The Add Client modal's immediate list refresh on success still works, and
  its status poll uses `DASHBOARD_PAIRING_POLL_INTERVAL_MS`.
- The previous version of this spec added table columns. Drop that; the
  graphs below replace it.

### Expected freshness

With defaults (sampler 500 ms, tick 1000 ms, poll 1000 ms), a value on screen
is typically under a second old, and up to roughly 1.5–2 s in the worst case,
plus network latency. To tighten this, lower `SAMPLER_INTERVAL_MS`,
`TICK_INTERVAL_MS` and `DASHBOARD_POLL_INTERVAL_MS`. A push channel (SSE) would
remove the poll delay entirely but isn't part of this spec.

### New component: `frontend/src/components/UsageGraph.tsx`

A classic Task Manager-style CPU history graph: dark background, green grid,
one bright green line, newest sample at the right edge, scrolling left as
samples arrive. The user supplied a reference image of the old Windows "CPU
Usage History" panel. Match its feel, but draw **only the green line** (the
reference's red kernel-time line is not wanted).

- Purely presentational. Suggested props: `values: (number | null)[]` (0–100,
  oldest first; `null` means no data for that slot), `capacity` (from
  `METRICS_HISTORY_SAMPLES`), `scrollOffset` (the latest sample's server
  `seq`, used to scroll the grid), `width`, `height`, `gridPx`, the three
  colours, `caption` and `ariaLabel`.
- Render with **SVG** and no chart library.
- Horizontal x spacing per sample = `width / (capacity - 1)`. When the buffer
  isn't full yet, the line starts partway across and ends at the right edge,
  like Task Manager filling in.
- Y: 0% at the bottom, 100% at the top, clamped, inset by 1 px so a 100% line
  stays visible.
- `null` values break the line into separate segments (gaps).
- Grid: horizontal lines fixed every `gridPx`. Vertical lines every `gridPx`,
  **shifting left in step with incoming samples** (offset by `scrollOffset`
  modulo the grid spacing in samples), so the grid scrolls with the data like
  the original.
- Grid lines use `shape-rendering="crispEdges"`; the usage line ~1.5 px wide.
- The graph keeps the same dark-screen colours in light and dark mode,
  because it's meant to look like a screen. Its container border and caption
  text follow the MUI theme.
- `role="img"` with an `aria-label` such as "CPU usage, 37 percent".
- Wrap in `React.memo`; many graphs re-render every second, so keep it cheap.

### Placement in the client card

The dashboard has evolved into per-client cards with a collapsed and an
expanded state since the original dashboard spec. Inspect the current card
component before changing it.

- In the **collapsed state**, put a strip of graphs **in the middle of the
  collapsed row**, between the existing left-side elements (status dot,
  hostname) and the right-side elements (dates, expand control).
- The strip shows, in order:
  - **CPU**: `cpu_percent`. Caption e.g. `CPU 37%`.
  - **RAM**: used % = (total − available) ÷ total. Caption e.g.
    `RAM 11.2 / 31.3 GiB`.
  - For each of the first `DASHBOARD_COLLAPSED_MAX_GPUS` GPUs:
    - **GPU**: `utilisation_percent`. Caption e.g. `GPU0 63% · 142 W`, with
      the power part shown only when present.
    - **VRAM**: used ÷ total. Caption e.g. `VRAM0 6.1 / 8.0 GiB`.
- Further GPUs, and swap, appear in the **expanded state**: reuse
  `UsageGraph` there, or plain text, whichever fits the current expanded
  layout.
- Sizes in GiB with one decimal.
- A disconnected client, or one with no samples yet, shows the empty grid with
  no line, slightly dimmed, with captions showing `–`. A client with no GPUs
  shows no GPU or VRAM graphs.
- At narrow widths, the graph strip **wraps onto its own line inside the
  card**. No horizontal page scrolling, and the existing elements must stay
  usable.
- Light/dark theming must keep working around the graphs.

## Docs to update

- `guide.md`: the new protocol messages and liveness model, the
  configuration variables, the GPU compose override, and the note about
  ANTON's separate `config.env`.
- `config.env.example` and `config.env`, as described above.

## Explicitly out of scope

- System, CPU or non-NVIDIA power (deferred). AMD and Intel GPUs.
- Slowing the tick on battery (deliberately deferred until it proves to be a
  problem).
- Smoothing or averaging metrics: spiky 1 s values are fine.
- Persisting metric history, alerts, or any push channel (SSE/WebSocket) to
  the browser.
- Hardware-based client identity (MAC address or machine ID); discussed and
  parked.

## Testing checklist

- [ ] `cargo build` and `cargo test` pass across the workspace. Unit tests
      cover: `Tick` with and without metrics, metrics validation (out-of-range,
      NaN, too many GPUs, long strings, too-close spacing), and config parsing
      including each cross-variable constraint.
- [ ] Server refuses to start with an invalid `config.env` value and names it.
      With an empty `config.env`, it starts on defaults and logs them.
- [ ] Change `TICK_INTERVAL_MS` to `2000`, restart the server: the client
      reconnects and ticks every 2 s (check server debug logs). Revert.
- [ ] **Sampler decoupling:** with the sampler artificially stalled (e.g. a
      test-only sleep), ticks keep arriving with `metrics: None` and the
      client never shows as disconnected.
- [ ] **Liveness:** `kill -9` the client: the dashboard dot goes red within
      about `LIVENESS_TIMEOUT_MS` plus one poll. Cut the client's network
      without closing the socket: the server still detects it. Stop the server
      container: the client notices within `SERVER_SILENCE_TIMEOUT_MS` and
      reconnects with backoff once the server is back.
- [ ] **Flush:** `last_seen_at` in SQLite advances about every 30 s while
      connected, immediately on disconnect, and on `docker stop` of the
      server.
- [ ] **Dashboard:** graphs scroll once a second with the grid moving in step.
      Reload the page: graphs repopulate from server history immediately.
      Hide the tab for a minute and return: polling resumes and the gap fills.
      Restart the server: the dashboard detects the new epoch and recovers
      without a reload.
- [ ] On a machine with an NVIDIA GPU, values roughly match `nvidia-smi` and
      the OS task manager while you load the CPU and GPU. On a machine without
      one, the client runs, reports CPU and RAM, and the card shows no GPU
      graphs, with no error spam.
- [ ] Docker client without the GPU override starts on a GPU-less host; with
      the override on a GPU host it reports the GPU.
- [ ] Card layout works in light and dark mode and at phone width (graphs
      wrap, no horizontal scroll), with several clients connected at once.
- [ ] Changing a `DASHBOARD_GRAPH_*` value and restarting the server changes
      the graphs without rebuilding the frontend.
- [ ] Full production test via the Docker image, then deploy to ANTON (adding
      the new variables to its `config.env`) and confirm the live dashboard
      at `https://llm.smith-c.com` shows your real client's graphs.
