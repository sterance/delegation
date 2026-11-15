// Chat data, mirroring server/src/conversations.rs, replies.rs, chat.rs and jobs/.

/**
 * A message's state; the same as `MessageStatus` on the server, and the
 * state of the reply's job. User messages are always `completed`. A reply
 * goes `queued` (waiting for a client) -> `assigned` (sent to a client) ->
 * `running` (first piece arrived) -> one of `completed`, `failed`,
 * `cancelled` (Stop) or `orphaned` (its client went away mid-reply and it
 * had no attempts left; the partial text is kept). A reply whose attempt is
 * lost goes back to `queued` and runs again.
 */
export type MessageStatus = 'queued' | 'assigned' | 'running' | 'completed' | 'failed' | 'cancelled' | 'orphaned'

export type Role = 'system' | 'user' | 'assistant'

/** Where a reply is before its text: the client is loading the model, or the model is working. */
export type Stage = 'loading_model' | 'generating'

export interface ChatMessage {
  id: string
  conversation_id: string
  /** Null for a first message. Siblings (same parent) are alternative branches. */
  parent_id: string | null
  role: Role
  content: string
  status: MessageStatus
  error: string | null
  attempts: number
  /** Null once the client that answered has been deleted. */
  client_id: string | null
  /** The client's name when it answered, kept after it's deleted. */
  client_label: string | null
  model: string | null
  created_at: string
  completed_at: string | null
  /** Live only, from the reply stream: never stored. */
  stage?: Stage | null
  /** Live only: where a queued reply stands. */
  queue?: QueueInfo | null
  /** Live only: the reply went back to the queue and is being tried again. */
  requeue?: RequeueNotice | null
  /** Extra facts about a reply: its usage (tokens, speed, what ran it) and history trimming. */
  meta?: MessageMeta | null
}

/** Where a loaded model runs; mirrors `RunInfo` in protocol/src/models.rs. */
export interface RunInfo {
  /** "cpu", "cuda-12", "cuda-13" or "vulkan". */
  variant: string
  /** Null for a build without GPU support. */
  gpu_layers: { offloaded: number; total: number } | null
  /** The GPU(s) used; null for CPU only. */
  device: string | null
}

export interface Usage {
  prompt_tokens: number
  completion_tokens: number
  tokens_per_second: number | null
  prompt_tokens_per_second: number | null
  run: RunInfo | null
}

export interface MessageMeta {
  usage?: Usage
  history_turns_dropped?: number
}

export interface ConversationSummary {
  id: string
  title: string
  /** The message whose branch is shown. */
  active_leaf_id: string | null
  created_at: string
  /** The sidebar sorts on it. */
  updated_at: string
}

export interface Conversation extends ConversationSummary {
  /** Every message, oldest first; the dashboard works out branches itself. */
  messages: ChatMessage[]
}

/** `GET /api/conversations`. */
export interface ConversationPage {
  conversations: ConversationSummary[]
  /** Pass as `before` for the next page; null on the last one. */
  next_before: string | null
}

/**
 * Who should answer: one client, or "any of my clients" that offers the
 * model (the next free one in the user's pool).
 */
export type Target = { kind: 'client'; client_id: string; model: string } | { kind: 'pool'; model: string }

/** Whether two targets name the same thing. */
export function sameTarget(a: Target, b: Target): boolean {
  if (a.model !== b.model) return false
  if (a.kind === 'client') return b.kind === 'client' && a.client_id === b.client_id
  return b.kind === 'pool'
}

/** Where a queued reply stands; mirrors `QueueInfo` in server/src/jobs/mod.rs. */
export interface QueueInfo {
  /** 1-based, among the queued jobs it could share a client with (approximate). */
  position: number
  /** Clients that could run it are connected but busy, or none is connected. */
  waiting_for: 'a_free_client' | 'a_client'
  /** The client a directed reply is waiting for; null for a pooled one. */
  client_label: string | null
}

/** Why an attempt ended without ending the reply. */
export type RequeueReason = 'lost' | 'timed_out' | 'refused' | 'interrupted'

/** The `requeued` event: the reply went back to the queue. */
export interface RequeueNotice {
  /** The attempt that comes next, from 1. */
  attempt: number
  max_attempts: number
  reason: RequeueReason
  previous_client_label: string | null
}

/** `POST /api/conversations/:id/messages`. Leave out `content` to regenerate the reply to `parent_id`. */
export interface SendRequest {
  content?: string
  parent_id: string | null
  target: Target
}

/** Who is answering a reply (or what it's waiting for), as `snapshot` and `status` events carry it. */
interface Answerer {
  client_id: string | null
  client_label: string | null
  model: string | null
  /** Set while the reply is queued, once the scheduler has looked at it. */
  queue?: QueueInfo | null
  /** Attempts made so far, the current one included. */
  attempt?: number
  max_attempts?: number | null
}

/** The server-sent events of a reply stream. */
export type ReplyEvent =
  | {
      type: 'created'
      /** Set when the send created the conversation. */
      conversation?: ConversationSummary | null
      /** Null when regenerating or retrying. */
      user_message?: ChatMessage | null
      reply: ChatMessage
    }
  | ({
      type: 'snapshot'
      message_id: string
      status: MessageStatus
      content: string
      error: string | null
      stage: Stage | null
    } & Answerer)
  | ({ type: 'status'; status: MessageStatus } & Answerer)
  | ({ type: 'requeued' } & RequeueNotice)
  | { type: 'progress'; stage: Stage }
  | { type: 'chunk'; delta: string }
  | { type: 'end'; status: MessageStatus; error: string | null; meta?: MessageMeta | null }

export type Backend = 'llama_cpp' | 'echo'

/** A model's state on its client; mirrors `ModelState` in protocol/src/models.rs. */
export type ModelState = 'downloading' | 'available' | 'loading' | 'loaded' | 'failed'

/** "Any of my clients" for one model: an entry of `pools` in `GET /api/chat/models`. */
export interface ChatPool {
  /** The model's id, as a pool target names it. */
  model: string
  model_name: string
  /** Connected clients in the user's pool that offer it. */
  clients: number
  /** How many of them have it loaded already. */
  loaded_on: number
}

/** `GET /api/chat/models`. */
export interface ChatModels {
  pools: ChatPool[]
  clients: ChatModel[]
}

/** One entry of `clients` in `GET /api/chat/models`: only models that can answer now. */
export interface ChatModel {
  client_id: string
  /** The client's friendly name (or hostname). */
  client_name: string
  /** The model's id, as a target names it. */
  model: string
  model_name: string
  backend: Backend
  state: ModelState
}

/** The picker's heading for pool entries, and what a pooled choice is called. */
export const POOL_LABEL = 'Any of my clients'

/** One entry in a model picker. */
export interface ModelOption {
  /** `pool/${model}` or `${clientId}/${model}`: unique across clients offering the same model. */
  id: string
  target: Target
  /** The model's display name. */
  label: string
  /** Which client hosts it, or "Any of my clients"; the picker groups by it. */
  detail: string
  /** For a pool entry: how many clients offer it, and whether it's loaded anywhere. */
  note?: string
}
