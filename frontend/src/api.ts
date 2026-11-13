// Thin wrappers over the server's dashboard-facing endpoints. Paths are
// always relative: in production this app is served same-origin by the
// Rust server, and in dev Vite proxies them (see vite.config.ts).
//
// Every call sends the session cookie. Any 401 (outside the login calls
// themselves) tells the app the session has gone, and it shows the login
// page; see setUnauthorizedHandler.

import type {
  Backend,
  ChatMessage,
  ChatModel,
  Conversation,
  ConversationPage,
  MessageStatus,
  ModelState,
  ReplyEvent,
  RunInfo,
  SendRequest,
  Target,
} from './types/chat'

export type AccessKind = 'admin' | 'owner'

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
  /** Whoever minted the pairing code it enrolled with. */
  owner: { user_id: string; username: string }
  access: AccessKind
  /** Decide what to offer from these, never from the user's role. */
  can_use: boolean
  can_manage: boolean
  /** Jobs (chat replies) it's working on now, whoever started them; the card's blue dot blinks while above 0. */
  active_jobs: number
  // From the client's `info` message: null until the client has connected once.
  os: string | null
  client_version: string | null
  specs: HardwareSpecs | null
  info_updated_at: string | null
  /** Samples newer than the request's `since`, oldest first. Always empty when disconnected. */
  samples: Sample[]
  /** What models it has; the card's expanded view fetches the details. */
  models: ModelsSummary
}

// ---- models on clients: mirrors protocol/src/models.rs and server/src/models ----

/** Counts by state (the built-in echo model left out), and pulls running. */
export interface ModelsSummary {
  available: number
  loading: number
  loaded: number
  downloading: number
  failed: number
  active_pulls: { command_id: string; model_id: string; model_name: string; progress: Progress | null }[]
}

export interface Progress {
  done_bytes: number
  total_bytes: number
  /** A pull downloads the llama.cpp runtime first if the client doesn't have it yet. */
  phase: 'runtime' | 'model'
}

export interface ModelMeta {
  family: string | null
  parameters: string | null
  quantization: string | null
}

export interface LocalModel {
  id: string
  display_name: string
  backend: Backend
  state: ModelState
  size_bytes: number
  context_length: number | null
  meta: ModelMeta
  /** Why it's `failed`. */
  error: string | null
  /** Missing from older clients. */
  settings?: ModelSettings
  /** Where it runs, while loaded. Missing from older clients. */
  run?: RunInfo | null
}

/** How many layers go to the GPU: llama.cpp's automatic fit, none, all, or a number. */
export type GpuLayers = 'auto' | 'off' | 'all' | number

export interface ModelSettings {
  gpu_layers: GpuLayers
  /** Null: the model's own. */
  context_length: number | null
}

export interface RuntimeInfo {
  backend: Backend
  version: string
  /** "cpu", "cuda-12", "cuda-13" or "vulkan". */
  variant: string
  state: 'absent' | 'downloading' | 'ready' | 'failed'
  // Missing from older clients (one CPU runtime, always the one used).
  /** The one models load with. */
  selected?: boolean
  /** Chosen from the hardware rather than by the dashboard. */
  automatic?: boolean
  devices?: string[]
  /** Why it failed. */
  error?: string | null
}

export type CommandState = 'sent' | 'accepted' | 'in_progress' | 'done' | 'failed' | 'cancelled' | 'unsupported'

export interface ModelCommand {
  command_id: string
  client_id: string
  kind: 'pull' | 'delete' | 'select_runtime' | 'configure'
  model_id: string
  model_name: string
  state: CommandState
  progress: Progress | null
  error: string | null
  requested_by: string
  started_at: string
  finished_at: string | null
}

/** `GET /api/clients/:id/models`. */
export interface ClientModels {
  client_id: string
  connected: boolean
  can_manage: boolean
  /** Empty until the client has reported, and while it's disconnected. */
  models: LocalModel[]
  storage: { models_bytes: number; free_bytes: number } | null
  runtimes: RuntimeInfo[]
  /** Recent and running commands, newest first; finished ones go after an hour. */
  commands: ModelCommand[]
}

export interface CatalogEntry {
  id: string
  display_name: string
  backend: Backend
  size_bytes: number
  context_length: number | null
  meta: ModelMeta
  /** The oldest llama.cpp release (e.g. "b12110") that can run it. */
  min_runtime_version: string | null
}

/** "b12110" -> 12110; null if it isn't a llama.cpp version. */
export function llamaCppBuild(version: string | null | undefined): number | null {
  const m = /^b(\d{1,9})$/.exec(version ?? '')
  return m ? Number(m[1]) : null
}

/** Why `entry` can't be installed on a client with these runtimes, or null if it can. */
export function runtimeTooOld(entry: CatalogEntry, runtimes: RuntimeInfo[]): string | null {
  const need = llamaCppBuild(entry.min_runtime_version)
  if (need === null) return null
  const builds = runtimes.map((r) => llamaCppBuild(r.version)).filter((b): b is number => b !== null)
  if (builds.length === 0) return null
  const have = Math.max(...builds)
  return have < need ? `Needs llama.cpp ${entry.min_runtime_version} or newer; this client has b${have}` : null
}

export function isCommandActive(state: CommandState): boolean {
  return state === 'sent' || state === 'accepted' || state === 'in_progress'
}

/** `GET /api/clients?since=<seq>`. */
export interface ClientsResponse {
  /** Random per server start. A change means the server restarted and `seq` numbering began again. */
  epoch: string
  /** The next poll's `since`. */
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
  /** One full bright-dim-bright cycle of the blue dot, which blinks while the client is working on a job. */
  blink_period_ms: number
  /** Samples kept per client, which is also each graph's width in samples. */
  metrics_history_samples: number
  tick_interval_ms: number
  pairing_code_ttl_secs: number
}

/** Used if `/api/dashboard-config` can't be loaded. Matches the server's compiled defaults. */
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

// ---- auth: mirrors server/src/auth ----

export type Role = 'admin' | 'user'

export interface User {
  id: string
  username: string
  role: Role
}

/** `GET /api/auth/status`. */
export interface AuthStatus {
  /** No admin yet: everyone goes to /setup. */
  setup_required: boolean
  signup_enabled: boolean
  password_min_len: number
  password_max_len: number
  /** SESSION_ABSOLUTE_TIMEOUT_DAYS, for "Remember me for N days". */
  remember_days: number
}

export interface UserSummary {
  id: string
  username: string
  role: Role
  created_at: string
  disabled_at: string | null
  client_count: number
  conversation_count: number
  last_active_at: string | null
}

export interface UsersResponse {
  signup_enabled: boolean
  /** Newest first. */
  users: UserSummary[]
}

export interface ResetLink {
  /** Shown once: the server only keeps its hash. */
  url: string
  expires_at: string
}

export class ApiError extends Error {
  readonly status: number

  constructor(status: number, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
  }
}

let unauthorized: (() => void) | null = null

/** Called on any 401 except from the sign-in calls, which use 401 for wrong passwords. */
export function setUnauthorizedHandler(handler: (() => void) | null): void {
  unauthorized = handler
}

/** The server's `{ error }` message, or a generic one. */
async function failure(res: Response, method: string, path: string): Promise<ApiError> {
  const body = (await res.json().catch(() => null)) as { error?: string } | null
  if (res.status === 401 && !path.startsWith('/api/auth/')) unauthorized?.()
  return new ApiError(res.status, body?.error ?? `${method} ${path} failed: ${res.status} ${res.statusText}`)
}

type Method = 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE'

async function request<T>(method: Method, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: {
      Accept: 'application/json',
      ...(body !== undefined && { 'Content-Type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    credentials: 'same-origin',
    // These are all live state; never serve them from cache.
    cache: 'no-store',
  })
  if (!res.ok) throw await failure(res, method, path)
  if (res.status === 204) return undefined as T
  return (await res.json()) as T
}

// ---- auth ----

export function getAuthStatus(): Promise<AuthStatus> {
  return request<AuthStatus>('GET', '/api/auth/status')
}

/** Null when signed out. */
export async function getMe(): Promise<User | null> {
  try {
    return await request<User>('GET', '/api/me')
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) return null
    throw err
  }
}

export function login(username: string, password: string, remember: boolean): Promise<User> {
  return request<User>('POST', '/api/auth/login', { username, password, remember })
}

export function signup(username: string, password: string, remember: boolean): Promise<User> {
  return request<User>('POST', '/api/auth/signup', { username, password, remember })
}

export function setup(token: string, username: string, password: string, remember: boolean): Promise<User> {
  return request<User>('POST', '/api/auth/setup', { token, username, password, remember })
}

export function logout(): Promise<void> {
  return request<void>('POST', '/api/auth/logout', {})
}

/** Signs out every other session. */
export function changePassword(currentPassword: string, newPassword: string): Promise<void> {
  return request<void>('POST', '/api/me/password', { current_password: currentPassword, new_password: newPassword })
}

export function getResetInfo(token: string): Promise<{ username: string; expires_at: string }> {
  return request('GET', `/api/password-reset/${encodeURIComponent(token)}`)
}

export function resetPassword(token: string, password: string): Promise<void> {
  return request<void>('POST', `/api/password-reset/${encodeURIComponent(token)}`, { password })
}

// ---- the admin ----

export function listUsers(): Promise<UsersResponse> {
  return request<UsersResponse>('GET', '/api/admin/users')
}

export function createResetLink(userId: string): Promise<ResetLink> {
  return request<ResetLink>('POST', `/api/admin/users/${encodeURIComponent(userId)}/reset-link`, {})
}

export function setUserDisabled(userId: string, disabled: boolean): Promise<void> {
  return request<void>('PATCH', `/api/admin/users/${encodeURIComponent(userId)}`, { disabled })
}

export function deleteUser(userId: string): Promise<void> {
  return request<void>('DELETE', `/api/admin/users/${encodeURIComponent(userId)}`)
}

// ---- clients ----

/** `since` 0 returns each connected client's full sample history. */
export function listClients(since: number): Promise<ClientsResponse> {
  return request<ClientsResponse>('GET', `/api/clients?since=${since}`)
}

/** Fields missing from the response (an older server) fall back to the defaults. */
export async function getDashboardConfig(): Promise<DashboardConfig> {
  const config = await request<Partial<DashboardConfig>>('GET', '/api/dashboard-config')
  return { ...DEFAULT_DASHBOARD_CONFIG, ...config }
}

/** Must match MAX_FRIENDLY_NAME_CHARS in server/src/clients.rs. */
export const MAX_FRIENDLY_NAME_LENGTH = 64

/** A blank name resets it to the client's hostname. */
export function renameClient(clientId: string, friendlyName: string): Promise<void> {
  return request<void>('PATCH', `/api/clients/${encodeURIComponent(clientId)}`, { friendly_name: friendlyName })
}

/** If the client is connected, the server drops it and the client forgets its identity. */
export function deleteClient(clientId: string): Promise<void> {
  return request<void>('DELETE', `/api/clients/${encodeURIComponent(clientId)}`)
}

/** Mints a new one-time pairing code, valid for `pairing_code_ttl_secs`. The client it enrolls is yours. */
export async function createPairingCode(): Promise<string> {
  const { code } = await request<{ code: string }>('POST', '/api/pairing-codes', {})
  return code
}

/** Throws an ApiError with status 404 if the server doesn't know the code. */
export function getPairingCodeStatus(code: string): Promise<PairingCodeStatus> {
  return request<PairingCodeStatus>('GET', `/api/pairing-codes/${encodeURIComponent(code)}/status`)
}

// ---- models on clients ----

/** What can be installed. */
export function listCatalog(): Promise<CatalogEntry[]> {
  return request<CatalogEntry[]>('GET', '/api/models/catalog')
}

export function getClientModels(clientId: string): Promise<ClientModels> {
  return request<ClientModels>('GET', `/api/clients/${encodeURIComponent(clientId)}/models`)
}

/** Sends a pull of a catalog entry. Rejects with a 409 if the client is offline. */
export function installModel(clientId: string, catalogId: string): Promise<ModelCommand> {
  return request<ModelCommand>('POST', `/api/clients/${encodeURIComponent(clientId)}/models`, {
    catalog_id: catalogId,
  })
}

export function deleteModel(clientId: string, modelId: string): Promise<ModelCommand> {
  return request<ModelCommand>(
    'DELETE',
    `/api/clients/${encodeURIComponent(clientId)}/models/${encodeURIComponent(modelId)}`,
  )
}

/** Chooses the llama.cpp variant models load with; "auto" lets the client choose from its hardware. */
export function selectRuntime(clientId: string, variant: string): Promise<ModelCommand> {
  return request<ModelCommand>('POST', `/api/clients/${encodeURIComponent(clientId)}/runtime`, { variant })
}

/** A model's settings; the client applies them the next time it loads the model. */
export function configureModel(clientId: string, modelId: string, settings: ModelSettings): Promise<ModelCommand> {
  return request<ModelCommand>(
    'PUT',
    `/api/clients/${encodeURIComponent(clientId)}/models/${encodeURIComponent(modelId)}/settings`,
    settings,
  )
}

/** Stops a download; the client keeps what it has, so installing again resumes. */
export function cancelModelCommand(clientId: string, commandId: string): Promise<ModelCommand> {
  return request<ModelCommand>(
    'POST',
    `/api/clients/${encodeURIComponent(clientId)}/model-commands/${encodeURIComponent(commandId)}/cancel`,
    {},
  )
}

// ---- chat ----

/** Models on connected clients you can chat with. */
export function listChatModels(): Promise<ChatModel[]> {
  return request<ChatModel[]>('GET', '/api/chat/models')
}

export function listConversations(before?: string | null): Promise<ConversationPage> {
  const query = before ? `?before=${encodeURIComponent(before)}` : ''
  return request<ConversationPage>('GET', `/api/conversations${query}`)
}

export function getConversation(id: string): Promise<Conversation> {
  return request<Conversation>('GET', `/api/conversations/${encodeURIComponent(id)}`)
}

export function updateConversation(id: string, patch: { title?: string; active_leaf_id?: string }): Promise<void> {
  return request<void>('PATCH', `/api/conversations/${encodeURIComponent(id)}`, patch)
}

export function deleteConversation(id: string): Promise<void> {
  return request<void>('DELETE', `/api/conversations/${encodeURIComponent(id)}`)
}

/** Stop: the reply keeps its text so far. */
export function cancelMessage(id: string): Promise<ChatMessage> {
  return request<ChatMessage>('POST', `/api/messages/${encodeURIComponent(id)}/cancel`, {})
}

/**
 * Streams server-sent events from a reply, calling `onEvent` for each, and
 * resolves after `end`. Rejects with the server's error if it refused before
 * streaming, or if the stream breaks off. Aborting `signal` only stops
 * listening: the reply keeps generating on the server.
 */
async function streamReply(
  method: 'GET' | 'POST',
  path: string,
  body: unknown,
  onEvent: (event: ReplyEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  const res = await fetch(path, {
    method,
    headers: {
      Accept: 'text/event-stream',
      ...(body !== undefined && { 'Content-Type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    credentials: 'same-origin',
    signal,
    cache: 'no-store',
  })
  if (!res.ok) throw await failure(res, method, path)
  if (!res.body) throw new Error('The reply had no body')

  // Blocks separated by a blank line, each with an `event:` and a `data:`
  // line. Lines starting with `:` are keep-alives. EventSource can't POST
  // (or be told not to reconnect), so this parses them by hand.
  const reader = res.body.pipeThrough(new TextDecoderStream()).getReader()
  let buffer = ''
  try {
    for (;;) {
      const { value, done } = await reader.read()
      if (done) throw new Error('The connection to the reply broke off')
      buffer += value.replace(/\r/g, '')
      let end: number
      while ((end = buffer.indexOf('\n\n')) !== -1) {
        const block = buffer.slice(0, end)
        buffer = buffer.slice(end + 2)
        let name = 'message'
        let data = ''
        for (const line of block.split('\n')) {
          if (line.startsWith('event:')) name = line.slice(6).trim()
          else if (line.startsWith('data:')) data += line.slice(5).trimStart()
        }
        if (!data) continue
        const event = { type: name, ...(JSON.parse(data) as object) } as ReplyEvent
        onEvent(event)
        if (event.type === 'end') return
      }
    }
  } finally {
    reader.cancel().catch(() => {})
  }
}

/** Sends a message (or, without `content`, regenerates the reply to `parent_id`) and streams the reply. */
export function sendMessage(
  conversationId: string,
  req: SendRequest,
  onEvent: (event: ReplyEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  return streamReply('POST', `/api/conversations/${encodeURIComponent(conversationId)}/messages`, req, onEvent, signal)
}

/** Reconnects to a reply: a snapshot, then what follows. */
export function streamMessage(id: string, onEvent: (event: ReplyEvent) => void, signal?: AbortSignal): Promise<void> {
  return streamReply('GET', `/api/messages/${encodeURIComponent(id)}/stream`, undefined, onEvent, signal)
}

/** A new attempt at a failed or interrupted reply; `target` defaults to the client that answered last time. */
export function retryMessage(
  id: string,
  target: Target | null,
  onEvent: (event: ReplyEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  return streamReply('POST', `/api/messages/${encodeURIComponent(id)}/retry`, target ? { target } : {}, onEvent, signal)
}

export function isActive(status: MessageStatus): boolean {
  return status === 'queued' || status === 'assigned' || status === 'running'
}

export function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err)
}
