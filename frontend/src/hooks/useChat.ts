import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from 'react'
import { errorMessage, listChatModels, streamChat } from '../api'
import type { ChatMessage, ChatTurn, Conversation, ModelOption } from '../types/chat'

// All chat state lives here, behind a small set of actions, so the
// components stay purely presentational. Conversations are in memory only:
// they last as long as the Chat page is mounted.

interface State {
  conversations: Conversation[]
  /** Null is a fresh "new chat": nothing is created until the first send. */
  activeId: string | null
  /** The user's pick. If it's gone from the model list, the first model is used instead. */
  modelId: string | null
}

type Action =
  | { type: 'select'; id: string | null }
  | { type: 'selectModel'; id: string }
  /** Appends to `conversationId`, creating it (titled from the message) if it doesn't exist. */
  | { type: 'append'; conversationId: string; message: ChatMessage }
  /** Merges into an existing message, e.g. to change its status. */
  | { type: 'patch'; conversationId: string; messageId: string; patch: Partial<ChatMessage> }
  /** Adds a streamed piece to the end of a message's content. */
  | { type: 'appendContent'; conversationId: string; messageId: string; delta: string }

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

function updateMessage(
  state: State,
  conversationId: string,
  messageId: string,
  update: (m: ChatMessage) => ChatMessage,
): State {
  return {
    ...state,
    conversations: state.conversations.map((c) =>
      c.id !== conversationId
        ? c
        : { ...c, messages: c.messages.map((m) => (m.id === messageId ? update(m) : m)) },
    ),
  }
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
      return updateMessage(state, action.conversationId, action.messageId, (m) => ({ ...m, ...action.patch }))
    case 'appendContent':
      return updateMessage(state, action.conversationId, action.messageId, (m) => ({
        ...m,
        content: m.content + action.delta,
      }))
  }
}

/** What the model sees: finished messages only, so failed replies aren't sent back as context. */
function historyOf(conversation: Conversation | null): ChatTurn[] {
  return (conversation?.messages ?? [])
    .filter((m) => m.status === 'done')
    .map((m) => ({ role: m.role, content: m.content }))
}

/** Models offered by connected clients, refetched on demand. */
function useModels(): [ModelOption[], () => void] {
  const [models, setModels] = useState<ModelOption[]>([])
  const refresh = useCallback(() => {
    listChatModels().then(
      (list) =>
        setModels(
          list.map((m) => ({
            id: `${m.client_id}/${m.model}`,
            clientId: m.client_id,
            model: m.model,
            label: m.model,
            detail: m.client_name,
          })),
        ),
      // Keep the last list: the picker still works if one refresh fails.
      (err: unknown) => console.error(`Couldn't load chat models: ${errorMessage(err)}`),
    )
  }, [])
  useEffect(refresh, [refresh])
  return [models, refresh]
}

export interface UseChat {
  /** Newest first. */
  conversations: Conversation[]
  active: Conversation | null
  models: ModelOption[]
  /** The model a send would use, or null if there are none. */
  modelId: string | null
  /** True while the active conversation is waiting on a reply; the composer disables send. */
  busy: boolean
  newChat: () => void
  selectChat: (id: string) => void
  selectModel: (id: string) => void
  /** Refetches the model list, e.g. when the picker opens. */
  refreshModels: () => void
  send: (text: string) => void
}

export function useChat(): UseChat {
  const [state, dispatch] = useReducer(reducer, { conversations: [], activeId: null, modelId: null })
  const [models, refreshModels] = useModels()

  // One per reply in flight, so leaving the page stops them all (and the
  // server then tells the client to stop generating).
  const inFlight = useRef(new Set<AbortController>())
  useEffect(() => {
    const controllers = inFlight.current
    return () => {
      for (const c of controllers) c.abort()
    }
  }, [])

  const active = state.conversations.find((c) => c.id === state.activeId) ?? null
  const busy = active?.messages.some((m) => m.status === 'pending') ?? false
  const model = models.find((m) => m.id === state.modelId) ?? models[0] ?? null

  const newChat = useCallback(() => dispatch({ type: 'select', id: null }), [])
  const selectChat = useCallback((id: string) => dispatch({ type: 'select', id }), [])
  const selectModel = useCallback((id: string) => dispatch({ type: 'selectModel', id }), [])

  const send = useCallback(
    (text: string) => {
      if (!model) return
      const conversationId = active?.id ?? newId()
      const messages = [...historyOf(active), { role: 'user' as const, content: text }]
      const now = new Date().toISOString()
      dispatch({
        type: 'append',
        conversationId,
        message: { id: newId(), role: 'user', content: text, createdAt: now, status: 'done' },
      })
      if (!active) dispatch({ type: 'select', id: conversationId })

      const replyId = newId()
      dispatch({
        type: 'append',
        conversationId,
        message: { id: replyId, role: 'assistant', content: '', createdAt: now, status: 'pending' },
      })
      const patch = (p: Partial<ChatMessage>) => dispatch({ type: 'patch', conversationId, messageId: replyId, patch: p })

      const controller = new AbortController()
      inFlight.current.add(controller)
      streamChat(
        { client_id: model.clientId, model: model.model, messages },
        (delta) => dispatch({ type: 'appendContent', conversationId, messageId: replyId, delta }),
        controller.signal,
      )
        .then(
          () => patch({ status: 'done' }),
          (err: unknown) => {
            // Aborted only when the page is going away; nothing to show.
            if (!controller.signal.aborted) patch({ status: 'error', error: errorMessage(err) })
          },
        )
        .finally(() => inFlight.current.delete(controller))
    },
    [active, model],
  )

  return useMemo(
    () => ({
      conversations: state.conversations,
      active,
      models,
      modelId: model?.id ?? null,
      busy,
      newChat,
      selectChat,
      selectModel,
      refreshModels,
      send,
    }),
    [state.conversations, active, models, model, busy, newChat, selectChat, selectModel, refreshModels, send],
  )
}
