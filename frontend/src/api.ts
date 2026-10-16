// Thin wrappers over the server's dashboard-facing endpoints. Paths are
// always relative: in production this app is served same-origin by the
// Rust server, and in dev Vite proxies them (see vite.config.ts).
//
// No auth here on purpose — Cloudflare Access sits in front of the whole
// origin, and same-origin fetches already carry its cookie.

export interface Client {
  client_id: string
  hostname: string
  /** What to display: the name set from the dashboard, or the hostname if none is. */
  friendly_name: string
  enrolled_at: string
  last_seen_at: string | null
  /** Live: true only while the client has an authenticated WebSocket open. */
  connected: boolean
  // From the client's `info` message: null until a client new enough to
  // send one has connected.
  os: string | null
  client_version: string | null
  specs: HardwareSpecs | null
  info_updated_at: string | null
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

export function listClients(): Promise<Client[]> {
  return request<Client[]>('GET', '/clients')
}

/** Must match MAX_FRIENDLY_NAME_CHARS in server/src/main.rs. */
export const MAX_FRIENDLY_NAME_LENGTH = 64

/** A blank name resets it to the client's hostname. */
export function renameClient(clientId: string, friendlyName: string): Promise<void> {
  return request<void>('PATCH', `/clients/${encodeURIComponent(clientId)}`, { friendly_name: friendlyName })
}

/** If the client is connected, the server drops it and the client forgets its identity. */
export function deleteClient(clientId: string): Promise<void> {
  return request<void>('DELETE', `/clients/${encodeURIComponent(clientId)}`)
}

/** Mints a new one-time pairing code, valid for 10 minutes. */
export async function createPairingCode(): Promise<string> {
  const { code } = await request<{ code: string }>('POST', '/admin/pairing-code')
  return code
}

/** Throws an ApiError with status 404 if the server doesn't know the code. */
export function getPairingCodeStatus(code: string): Promise<PairingCodeStatus> {
  return request<PairingCodeStatus>('GET', `/admin/pairing-code/${encodeURIComponent(code)}/status`)
}

export function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err)
}
