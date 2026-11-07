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

/** One entry in the model picker. Will eventually be "a model on a client". */
export interface ModelOption {
  id: string
  label: string
  /** Secondary line, e.g. which client hosts it. */
  detail?: string
}
