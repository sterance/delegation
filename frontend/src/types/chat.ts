// Chat data, mirroring server/src/conversations.rs, replies.rs and chat.rs.

/**
 * A message's state; the same as `MessageStatus` on the server. User
 * messages are always `completed`. A reply goes `queued` (created) ->
 * `assigned` (sent to a client) -> `running` (first piece arrived) -> one of
 * `completed`, `failed`, `cancelled` (Stop) or `orphaned` (its client went
 * away mid-reply; the partial text is kept).
 */
export type MessageStatus = 'queued' | 'assigned' | 'running' | 'completed' | 'failed' | 'cancelled' | 'orphaned'

export type Role = 'system' | 'user' | 'assistant'

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

/** Who should answer. Only `client` exists for now. */
export interface Target {
  kind: 'client'
  client_id: string
  model: string
}

/** `POST /api/conversations/:id/messages`. Leave out `content` to regenerate the reply to `parent_id`. */
export interface SendRequest {
  content?: string
  parent_id: string | null
  target: Target
}

/** Who is answering a reply, as `snapshot` and `status` events carry it. */
interface Answerer {
  client_id: string | null
  client_label: string | null
  model: string | null
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
  | ({ type: 'snapshot'; message_id: string; status: MessageStatus; content: string; error: string | null } & Answerer)
  | ({ type: 'status'; status: MessageStatus } & Answerer)
  | { type: 'chunk'; delta: string }
  | { type: 'end'; status: MessageStatus; error: string | null }

/** One entry of `GET /api/chat/models`. */
export interface ChatModel {
  client_id: string
  /** The client's friendly name (or hostname). */
  client_name: string
  model: string
}

/** One entry in a model picker. */
export interface ModelOption {
  /** `${clientId}/${model}`: unique across clients offering the same model. */
  id: string
  target: Target
  /** The model name. */
  label: string
  /** Secondary line: which client hosts it. */
  detail: string
}
