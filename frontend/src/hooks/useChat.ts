import { useCallback, useMemo, useReducer } from 'react'
import { MOCK_CONVERSATIONS, MOCK_MODELS } from '../utils/mockChat'
import type { ChatMessage, Conversation, ModelOption } from '../types/chat'

// All chat state lives here, behind a small set of actions, so the
// components stay purely presentational. Wiring up a backend means changing
// `send` (and later, loading conversations/models) without touching the UI.

interface State {
  conversations: Conversation[]
  /** Null is a fresh "new chat": nothing is created until the first send. */
  activeId: string | null
  modelId: string | null
}

type Action =
  | { type: 'select'; id: string | null }
  | { type: 'selectModel'; id: string }
  /** Appends to `conversationId`, creating it (titled from the message) if it doesn't exist. */
  | { type: 'append'; conversationId: string; message: ChatMessage }
  /** Merges into an existing message: for status changes and, later, streamed chunks. */
  | { type: 'patch'; conversationId: string; messageId: string; patch: Partial<ChatMessage> }

const TITLE_MAX = 60

/** crypto.randomUUID only exists in secure contexts (HTTPS/localhost), so fall back on plain-HTTP LAN access. */
function newId(): string {
  return typeof crypto.randomUUID === 'function'
    ? crypto.randomUUID()
    : `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`
}

function titleFrom(text: string): string {
  const line = text.trim().split('\n')[0]
  return line.length > TITLE_MAX ? `${line.slice(0, TITLE_MAX)}…` : line || 'New chat'
}

function reducer(state: State, action: Action): State {
  switch (action.type) {
    case 'select':
      return { ...state, activeId: action.id }
    case 'selectModel':
      return { ...state, modelId: action.id }
    case 'append': {
      const { conversationId, message } = action
      const existing = state.conversations.find((c) => c.id === conversationId)
      const updated: Conversation = existing
        ? { ...existing, updatedAt: message.createdAt, messages: [...existing.messages, message] }
        : { id: conversationId, title: titleFrom(message.content), updatedAt: message.createdAt, messages: [message] }
      return {
        ...state,
        conversations: [updated, ...state.conversations.filter((c) => c.id !== conversationId)],
      }
    }
    case 'patch':
      return {
        ...state,
        conversations: state.conversations.map((c) =>
          c.id !== action.conversationId
            ? c
            : {
                ...c,
                messages: c.messages.map((m) => (m.id === action.messageId ? { ...m, ...action.patch } : m)),
              },
        ),
      }
  }
}

export interface UseChat {
  /** Newest first. */
  conversations: Conversation[]
  active: Conversation | null
  models: ModelOption[]
  modelId: string | null
  /** True while the active conversation is waiting on a reply; the composer disables send. */
  busy: boolean
  newChat: () => void
  selectChat: (id: string) => void
  selectModel: (id: string) => void
  send: (text: string) => void
}

export function useChat(): UseChat {
  const [state, dispatch] = useReducer(reducer, {
    conversations: MOCK_CONVERSATIONS,
    activeId: MOCK_CONVERSATIONS[0]?.id ?? null,
    modelId: MOCK_MODELS[0]?.id ?? null,
  })

  const active = state.conversations.find((c) => c.id === state.activeId) ?? null
  const busy = active?.messages.some((m) => m.status === 'pending') ?? false

  const newChat = useCallback(() => dispatch({ type: 'select', id: null }), [])
  const selectChat = useCallback((id: string) => dispatch({ type: 'select', id }), [])
  const selectModel = useCallback((id: string) => dispatch({ type: 'selectModel', id }), [])

  const send = useCallback(
    (text: string) => {
      const conversationId = state.activeId ?? newId()
      const userMessage: ChatMessage = {
        id: newId(),
        role: 'user',
        content: text,
        createdAt: new Date().toISOString(),
        status: 'done',
      }
      dispatch({ type: 'append', conversationId, message: userMessage })
      if (state.activeId === null) dispatch({ type: 'select', id: conversationId })

      // UI-only for now: nothing is sent anywhere. Next step, roughly:
      //   1. append an assistant message { content: '', status: 'pending' }
      //   2. send `text` (+ state.modelId) to the client
      //   3. on reply: patch it to { content, status: 'done' }
      //      (or append chunks via repeated patches when streaming)
      //   4. on failure: patch it to { status: 'error', error }
    },
    [state.activeId],
  )

  return useMemo(
    () => ({
      conversations: state.conversations,
      active,
      models: MOCK_MODELS,
      modelId: state.modelId,
      busy,
      newChat,
      selectChat,
      selectModel,
      send,
    }),
    [state.conversations, active, state.modelId, busy, newChat, selectChat, selectModel, send],
  )
}
