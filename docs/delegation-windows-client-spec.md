# Build the Windows tray client — instructions for Claude Code

## Context

This is the delegation project: a Rust/axum server (`server/`), a Rust client
(`client/`), a shared protocol crate (`protocol/`), and a React dashboard
(`frontend/`), all in one Cargo workspace. The server and the headless Linux
client are already built and tested end-to-end against the live deployment at
`llm.smith-c.com` (behind a Cloudflare Tunnel; Cloudflare Access protects the
dashboard but bypasses `/ws` and `/enroll`). Read `guide.md` and
`dev/local-test.md` at the repo root first. This document only covers the new
piece: the Windows tray front end for the client.

Recent renames to be aware of: the WebSocket route is `/ws` (was `/agent`), and
the binary/prose say "client" (was "agent"). Check the current code for exact
names (binary, config directory) rather than trusting this document.

**Do not change the wire protocol in `protocol/`, or the server.** The existing
enrollment, Ed25519 challenge-response, and heartbeat logic in `client/` is
proven. Reuse it; don't rewrite it. The headless Linux client (and its Docker
image) must keep working.

## What you're building

A Windows system-tray app. No main window, no daemon, no Windows Service: a
per-user background app that starts at login and shows only a tray icon with a
context menu, plus small popup forms for text input.

It is a *tray app, not a Service* on purpose: Services run in Session 0, which
cannot show a tray icon. Auto-start is via the per-user registry `Run` key
(`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`) or a Startup shortcut.

## Connection model

- The client holds **one active connection at a time**, using one single
  connection task. There are no concurrent per-server loops.
- It keeps an ordered **history of saved servers**. Each entry has its own
  `{ url, client_id, signing_key }` (a distinct Ed25519 keypair per server, so
  servers can't correlate you by public key).
- Exactly one entry is marked **active**. Also persist whether the user last
  *manually disconnected* it.
- Switching the active server tears down the current connection and starts one
  against the new entry's identity.

## Tray menu

- One row per saved server, showing its URL.
- The **active** row shows a green/red dot for connected/disconnected (same
  green/red convention as the dashboard's client list). Clicking it toggles
  connect/disconnect.
- Clicking a **historical (non-active)** row makes it active and connects
  immediately. No pairing code needed, since it already has an identity.
- **"Add server…"** is pinned at the bottom (opens the form below).
- A "Quit" item is needed to exit the app (my addition, not in the original
  brief; keep it minimal).
- Only two states are ever shown: connected / disconnected. There is no
  "awaiting verification" state.
- Update item text/icons in place on state changes rather than rebuilding the
  whole menu on every heartbeat (`muda` supports mutating items).

## "Add server…" form

A minimal GUI popup (egui/eframe or the most minimal Rust option that works):

1. Input for the server URL.
2. If the URL already has a saved identity, skip the pairing code and just make
   it active and connect. This is a purely local check, with no network
   round-trip.
3. Otherwise show a **pairing code** input. The admin gets the code from the
   dashboard's "Add client" modal (valid 10 minutes, one-time).
4. On submit: enroll via `POST /enroll` (existing client logic), save the new
   entry, make it active, connect. The pairing code is never stored; it is
   consumed server-side.

Show clear errors inline (bad URL, bad/expired code, server unreachable).

## Self-heal

If the server rejects a saved identity (`AuthFailed` / unknown `client_id`,
which can arrive right after `Hello` *or* after the challenge; the Linux client
already handles both points), clear **that entry's** identity, stop retrying,
and prompt for a pairing code for that URL (reuse the form, pre-filled).
Don't loop forever on a handshake that can't succeed.

## Persistence and restart behavior

- Store the history + active marker + manual-disconnect flag in a local config
  file under the Windows user config dir (`%APPDATA%`; use the `dirs` crate or
  equivalent). All storage is local to the machine; nothing is stored
  server-side for this.
- On launch: resume the same server as active. Auto-connect **unless** it was
  manually disconnected when the app last closed.
- On first add of a server, default to connected.

## Suggested technical approach (override if you find better)

- `tray-icon` (+ `muda`) for the tray and menu.
- `egui`/`eframe` for the form. Running an egui window alongside the tray event
  loop can conflict; if so, a simple, robust option is to launch the form as a
  short-lived child process of the same exe (e.g. `--add-server-form <url?>`)
  that returns its result over stdout. Your call, but keep it simple.
- Refactor `client/` so the connection logic is a reusable library module
  (status-reporting channel out, connect/disconnect commands in), with the
  existing CLI and the new tray as two front ends. A separate binary target or
  a `windows`-gated feature are both fine. Don't duplicate protocol code.
- Keep dependencies minimal, matching the rest of the project (it chose `ureq`
  over `reqwest` for the same reason).
- Release builds should use the Windows GUI subsystem
  (`#![windows_subsystem = "windows"]`) so no console window appears.

## Explicitly out of scope

- Removing saved servers (edit the config file by hand for now).
- Windows Service / true daemon mode.
- MSI/installer packaging, auto-update, toast notifications.
- Any login/auth UI (the pairing code and keypair are the auth).
- Multiple simultaneous connections.
- Changes to the server, `protocol/`, or the dashboard.

## Open questions to ask the user before building

- Where will this be built and tested? Their dev laptop (`lappy`) is Linux, so
  either cross-compile (`x86_64-pc-windows-gnu`, needs mingw; watch TLS/C
  dependencies) or build on a Windows machine. Ask which, and whether they
  have a Windows machine to run the tray app on.
- Should the tray app self-register for auto-start on first run, or is that a
  manual step for now? (I'd assume self-register via the `Run` key.)

## Testing checklist

- [ ] Workspace still builds; the Linux headless client and its Docker image
      still work unchanged.
- [ ] Tray app launches with no console window and shows the menu.
- [ ] Add a server through the form using a real pairing code from the live
      dashboard; the client appears there with the right hostname and a green
      dot.
- [ ] Toggle disconnect/connect from the tray; the dashboard dot follows.
- [ ] Add a second server; switching between them works and only the active
      row shows a dot.
- [ ] Quit and relaunch: the same server resumes as active; a manually
      disconnected one stays disconnected.
- [ ] Wipe the server's DB (or use a stale identity): the client self-heals
      into the pairing prompt instead of looping.
- [ ] Reboot/log in again and confirm it auto-starts.
