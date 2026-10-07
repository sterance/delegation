// Thin wrappers over the server's dashboard-facing endpoints. Paths are
// always relative: in production this app is served same-origin by the
// Rust server, and in dev Vite proxies them (see vite.config.ts).
//
// No auth here on purpose — Cloudflare Access sits in front of the whole
// origin, and same-origin fetches already carry its cookie.

export interface Client {
  client_id: string
  hostname: string
  enrolled_at: string
  last_seen_at: string | null
  /** Live: true only while the client has an authenticated WebSocket open. */
  connected: boolean
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

async function request<T>(method: 'GET' | 'POST', path: string): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: { Accept: 'application/json' },
    // These are all polled for live state; never serve them from cache.
    cache: 'no-store',
  })
  if (!res.ok) {
    throw new ApiError(res.status, `${method} ${path} failed: ${res.status} ${res.statusText}`)
  }
  return (await res.json()) as T
}

export function listClients(): Promise<Client[]> {
  return request<Client[]>('GET', '/clients')
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
