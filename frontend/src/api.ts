// Thin wrappers over the server's dashboard-facing endpoints. Paths are
// always relative: in production this app is served same-origin by the
// Rust server, and in dev Vite proxies them (see vite.config.ts).
//
// No auth here on purpose — Cloudflare Access sits in front of the whole
// origin, and same-origin fetches already carry its cookie.

import type { ChatModel, ChatRequest } from './types/chat'

export interface Client {
  client_id: string
  hostname: string
  /** What to display: the name set from the dashboard, or the hostname if none is. */
  friendly_name: string
  enrolled_at: string
  /** The server's in-memory value, so it's current even between its database flushes. */
  last_seen_at: string | null
  /** Live: true only while the client has an authenticated WebSocket open. */
  connected: boolean
  // From the client's `info` message: null until the client has connected once.
  os: string | null
  client_version: string | null
  specs: HardwareSpecs | null
  info_updated_at: string | null
  /** Samples newer than the request's `since`, oldest first. Always empty when disconnected. */
  samples: Sample[]
}

/** `GET /api/clients?since=<seq>`. */
export interface ClientsResponse {
  /** Random per server start. A change means the server restarted and `seq` numbering began again. */
  epoch: string
  /** Newest sample seq across all clients: the next poll's `since`. */
  latest_seq: number
  clients: Client[]
}

// Mirror of `Sample` in server/src/live.rs (built from the tick `Metrics`
// in protocol/src/telemetry.rs). Raw units.
export interface Sample {
  /** Server-wide sequence number, shared by all clients. */
  seq: number
  /** When the server received it, e.g. "2026-11-06T12:01:08.412Z". */
  received_at: string
  cpu_percent: number
  ram_total_bytes: number
  ram_available_bytes: number
  swap_total_bytes: number | null
  swap_used_bytes: number | null
  /** NVIDIA GPUs only; empty when there are none. */
  gpus: GpuSample[]
}

export interface GpuSample {
  name: string
  utilisation_percent: number
  vram_used_bytes: number
  vram_total_bytes: number
  /** Null when the driver doesn't report power for this GPU. */
  power_watts: number | null
}

// Mirror of `DashboardConfig` in server/src/config.rs: the DASHBOARD_* values
// from config.env (prefix dropped, lowercased) plus a few others the
// dashboard needs.
export interface DashboardConfig {
  poll_interval_ms: number
  pause_when_hidden: boolean
  pairing_poll_interval_ms: number
  graph_width_px: number
  graph_height_px: number
  graph_grid_px: number
  graph_line_color: string
  graph_grid_color: string
  graph_bg_color: string
  collapsed_max_gpus: number
  /** One full bright-dim-bright cycle of the blue dot. */
  blink_period_ms: number
  /** Samples kept per client, which is also each graph's width in samples. */
  metrics_history_samples: number
  tick_interval_ms: number
  pairing_code_ttl_secs: number
}

/** Used if `/api/admin/dashboard-config` can't be loaded. Matches the server's compiled defaults. */
export const DEFAULT_DASHBOARD_CONFIG: DashboardConfig = {
  poll_interval_ms: 1000,
  pause_when_hidden: true,
  pairing_poll_interval_ms: 1500,
  graph_width_px: 120,
  graph_height_px: 40,
  graph_grid_px: 10,
  graph_line_color: '#00ff00',
  graph_grid_color: '#008040',
  graph_bg_color: '#000000',
  collapsed_max_gpus: 1,
  blink_period_ms: 1000,
  metrics_history_samples: 60,
  tick_interval_ms: 1000,
  pairing_code_ttl_secs: 600,
}

// Hand-written mirror of `HardwareSpecs` in protocol/src/hardware.rs; keep
// the two in sync. Everything is raw units (bytes, MHz, MT/s) and any field
// may be missing when the client couldn't read it.
export interface HardwareSpecs {
  cpu: Cpu | null
  memory: Memory | null
  gpus: Gpu[]
  disks: Disk[]
}

export interface Cpu {
  model: string | null
  max_mhz: number | null
  physical_cores: number | null
  logical_cores: number | null
}

export interface Memory {
  total_bytes: number | null
  /** e.g. "DDR5". Often missing on Linux clients not running as root. */
  standard: string | null
  speed_mts: number | null
  module_count: number | null
}

export interface Gpu {
  model: string | null
  vendor: string | null
  vram_bytes: number | null
  /** Integrated GPU using system RAM. */
  vram_shared: boolean
  max_core_mhz: number | null
}

export type DiskMedia = 'hdd' | 'ssd' | 'unknown'
export type DiskInterface = 'nvme' | 'sata' | 'sas' | 'usb' | 'virtual' | 'other'

export interface Disk {
  model: string | null
  media: DiskMedia
  interface: DiskInterface
  total_bytes: number | null
  available_bytes: number | null
}

export interface ClaimedBy {
  client_id: string
  hostname: string
}

export interface PairingCodeStatus {
  used: boolean
  claimed_by: ClaimedBy | null
}

export class ApiError extends Error {
  readonly status: number

  constructor(status: number, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
  }
}

async function request<T>(method: 'GET' | 'POST' | 'PATCH' | 'DELETE', path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: {
      Accept: 'application/json',
      ...(body !== undefined && { 'Content-Type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    // These are all polled for live state; never serve them from cache.
    cache: 'no-store',
  })
  if (!res.ok) {
    throw new ApiError(res.status, `${method} ${path} failed: ${res.status} ${res.statusText}`)
  }
  if (res.status === 204) return undefined as T
  return (await res.json()) as T
}

/** `since` 0 returns each connected client's full sample history. */
export function listClients(since: number): Promise<ClientsResponse> {
  return request<ClientsResponse>('GET', `/api/clients?since=${since}`)
}

/** Fields missing from the response (an older server) fall back to the defaults. */
export async function getDashboardConfig(): Promise<DashboardConfig> {
  const config = await request<Partial<DashboardConfig>>('GET', '/api/admin/dashboard-config')
  return { ...DEFAULT_DASHBOARD_CONFIG, ...config }
}

/** Must match MAX_FRIENDLY_NAME_CHARS in server/src/main.rs. */
export const MAX_FRIENDLY_NAME_LENGTH = 64

/** A blank name resets it to the client's hostname. */
export function renameClient(clientId: string, friendlyName: string): Promise<void> {
  return request<void>('PATCH', `/api/clients/${encodeURIComponent(clientId)}`, { friendly_name: friendlyName })
}

/** If the client is connected, the server drops it and the client forgets its identity. */
export function deleteClient(clientId: string): Promise<void> {
  return request<void>('DELETE', `/api/clients/${encodeURIComponent(clientId)}`)
}

/** Mints a new one-time pairing code, valid for `pairing_code_ttl_secs`. */
export async function createPairingCode(): Promise<string> {
  const { code } = await request<{ code: string }>('POST', '/api/admin/pairing-code')
  return code
}

/** Throws an ApiError with status 404 if the server doesn't know the code. */
export function getPairingCodeStatus(code: string): Promise<PairingCodeStatus> {
  return request<PairingCodeStatus>('GET', `/api/admin/pairing-code/${encodeURIComponent(code)}/status`)
}

/** Every model offered by a connected client. */
export function listChatModels(): Promise<ChatModel[]> {
  return request<ChatModel[]>('GET', '/api/chat/models')
}

/**
 * `POST /api/chat`: streams the reply, calling `onDelta` with each piece as
 * it arrives. Resolves once the reply is complete; rejects with the
 * server's or client's error, or if the stream breaks off. Aborting
 * `signal` stops it, and the server then cancels the request on the client.
 */
export async function streamChat(req: ChatRequest, onDelta: (delta: string) => void, signal?: AbortSignal): Promise<void> {
  const res = await fetch('/api/chat', {
    method: 'POST',
    headers: { Accept: 'text/event-stream', 'Content-Type': 'application/json' },
    body: JSON.stringify(req),
    signal,
    cache: 'no-store',
  })
  if (!res.ok) {
    // Errors found before the reply starts come back as JSON `{error}`.
    const body = (await res.json().catch(() => null)) as { error?: string } | null
    throw new ApiError(res.status, body?.error ?? `POST /api/chat failed: ${res.status} ${res.statusText}`)
  }
  if (!res.body) throw new Error('The reply had no body')

  // Server-sent events: blocks separated by a blank line, each with an
  // `event:` and a `data:` line. Lines starting with `:` are keep-alives.
  // EventSource can't POST, so this parses them by hand.
  const reader = res.body.pipeThrough(new TextDecoderStream()).getReader()
  let buffer = ''
  try {
    for (;;) {
      const { value, done } = await reader.read()
      if (done) throw new Error('The reply broke off before it finished')
      buffer += value.replace(/\r/g, '')
      let end: number
      while ((end = buffer.indexOf('\n\n')) !== -1) {
        const block = buffer.slice(0, end)
        buffer = buffer.slice(end + 2)
        let event = 'message'
        let data = ''
        for (const line of block.split('\n')) {
          if (line.startsWith('event:')) event = line.slice(6).trim()
          else if (line.startsWith('data:')) data += line.slice(5).trimStart()
        }
        if (!data) continue
        const payload = JSON.parse(data) as { delta?: string; error?: string }
        if (event === 'chunk') onDelta(payload.delta ?? '')
        else if (event === 'done') return
        else if (event === 'error') throw new Error(payload.error ?? 'The reply failed')
      }
    }
  } finally {
    reader.cancel().catch(() => {})
  }
}

export function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err)
}
