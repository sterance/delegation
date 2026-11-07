// Chat data model, kept independent of any transport so the UI doesn't care
// whether a reply comes from a client echo, a real LLM, or a stream.

export type Role = 'user' | 'assistant'

/**
 * `pending`: sent, waiting on (or still streaming) a reply.
 * `error`: the send or the reply failed; `error` says why.
 */
export type MessageStatus = 'done' | 'pending' | 'error'

export interface ChatMessage {
  id: string
  role: Role
  content: string
  /** ISO timestamp. */
  createdAt: string
  status: MessageStatus
  error?: string
}

export interface Conversation {
  id: string
  title: string
  /** ISO timestamp of the latest message; the sidebar sorts on it. */
  updatedAt: string
  messages: ChatMessage[]
}

/** One entry in the model picker: a model offered by a connected client. */
export interface ModelOption {
  /** `${clientId}/${model}`: unique across clients offering the same model. */
  id: string
  clientId: string
  model: string
  /** The model name. */
  label: string
  /** Secondary line: which client hosts it. */
  detail?: string
}

// ---- wire types: mirror server/src/chat.rs and protocol/src/chat.rs ----

/** One message of the history sent with a request. */
export interface ChatTurn {
  role: Role
  content: string
}

/** `POST /api/chat` body. `messages` is the whole conversation, ending with the new message. */
export interface ChatRequest {
  client_id: string
  model: string
  messages: ChatTurn[]
}

/** One entry of `GET /api/chat/models`. */
export interface ChatModel {
  client_id: string
  /** The client's friendly name (or hostname). */
  client_name: string
  model: string
}
