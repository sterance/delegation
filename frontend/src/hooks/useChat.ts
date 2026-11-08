import { useCallback, useEffect, useLayoutEffect, useMemo, useReducer, useRef, useState } from 'react'
import {
  ApiError,
  cancelMessage,
  deleteConversation,
  errorMessage,
  getConversation,
  isActive,
  listChatModels,
  listConversations,
  retryMessage,
  sendMessage,
  streamMessage,
  updateConversation,
} from '../api'
import type { ChatMessage, Conversation, ConversationSummary, ModelOption, ReplyEvent, Target } from '../types/chat'
import { branch, childIndex, latestLeaf, type BranchStep } from '../utils/tree'

// All chat state lives here, behind a small set of actions, so the
// components stay presentational. Conversations live on the server: this
// keeps the sidebar list and the open conversation, updates both at once
// when sending, and reconciles with the server's ids and statuses as the
// reply streams in. Replies keep generating on the server whatever happens
// here; leaving the page or switching chats only stops listening.

interface Open {
  id: string
  /** A new chat whose first send hasn't been confirmed yet: nothing to fetch. */
  local: boolean
  loading: boolean
  loadError: string | null
  title: string
  activeLeafId: string | null
  messages: ReadonlyMap<string, ChatMessage>
}

interface State {
  /** Newest first. */
  conversations: ConversationSummary[]
  nextBefore: string | null
  listError: string | null
  /** Null is a fresh "new chat": nothing is created until the first send. */
  open: Open | null
  /** The user's pick. If it's gone from the model list, the first model is used instead. */
  modelId: string | null
}

type Action =
  | { type: 'listLoaded'; conversations: ConversationSummary[]; nextBefore: string | null; append: boolean }
  | { type: 'listFailed'; error: string }
  | { type: 'newChat' }
  | { type: 'openChat'; id: string }
  | { type: 'loaded'; conversation: Conversation }
  | { type: 'loadFailed'; id: string; error: string }
  /** A brand-new chat's first send. */
  | { type: 'startLocal'; id: string; title: string; messages: ChatMessage[]; leafId: string }
  /** Adds or replaces messages by id, optionally moving the leaf. */
  | { type: 'upsert'; conversationId: string; messages: ChatMessage[]; leafId?: string | null }
  | { type: 'remove'; conversationId: string; ids: string[] }
  | { type: 'setLeaf'; conversationId: string; leafId: string | null }
  | { type: 'event'; conversationId: string; messageId: string; event: ReplyEvent }
  | { type: 'touched'; summary: ConversationSummary }
  | { type: 'renamed'; id: string; title: string }
  | { type: 'deleted'; id: string }
  | { type: 'selectModel'; id: string }

const TITLE_MAX = 60
const TEMP = 'tmp-'
const RECONNECT_DELAY_MS = 1500

/** A v4 UUID; crypto.randomUUID only exists in secure contexts (HTTPS/localhost). */
function uuid(): string {
  if (typeof crypto.randomUUID === 'function') return crypto.randomUUID()
  const b = crypto.getRandomValues(new Uint8Array(16))
  b[6] = (b[6] & 0x0f) | 0x40
  b[8] = (b[8] & 0x3f) | 0x80
  const h = Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('')
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`
}

/** Same rule as the server's `title_from`, for the instant sidebar entry. */
function titleFrom(text: string): string {
  const line = text.trim().split('\n')[0].trim()
  if (!line) return 'New chat'
  return [...line].length > TITLE_MAX ? `${[...line].slice(0, TITLE_MAX).join('')}…` : line
}

function temp(conversationId: string, role: 'user' | 'assistant', parentId: string | null, content: string, target?: ModelOption): ChatMessage {
  const now = new Date().toISOString()
  return {
    id: `${TEMP}${uuid()}`,
    conversation_id: conversationId,
    parent_id: parentId,
    role,
    content,
    status: role === 'user' ? 'completed' : 'queued',
    error: null,
    attempts: 0,
    client_id: target?.target.client_id ?? null,
    client_label: target?.detail ?? null,
    model: target?.target.model ?? null,
    created_at: now,
    completed_at: role === 'user' ? now : null,
  }
}

function withOpen(state: State, conversationId: string, update: (open: Open) => Open): State {
  if (state.open?.id !== conversationId) return state
  return { ...state, open: update(state.open) }
}

function applyEvent(m: ChatMessage, event: ReplyEvent): ChatMessage {
  switch (event.type) {
    case 'snapshot':
      return {
        ...m,
        content: event.content,
        status: event.status,
        error: event.error,
        client_id: event.client_id ?? m.client_id,
        client_label: event.client_label ?? m.client_label,
        model: event.model ?? m.model,
      }
    case 'status':
      return {
        ...m,
        status: event.status,
        client_id: event.client_id ?? m.client_id,
        client_label: event.client_label ?? m.client_label,
        model: event.model ?? m.model,
      }
    case 'chunk':
      return { ...m, content: m.content + event.delta }
    case 'end':
      return { ...m, status: event.status, error: event.error, completed_at: new Date().toISOString() }
    case 'created':
      return m
  }
}

function reducer(state: State, action: Action): State {
  switch (action.type) {
    case 'listLoaded': {
      const conversations = action.append ? [...state.conversations, ...action.conversations] : action.conversations
      // A chat whose first send is still in flight isn't on the server's list yet.
      const local = state.open?.local ? state.conversations.find((c) => c.id === state.open?.id) : undefined
      const merged = local && !conversations.some((c) => c.id === local.id) ? [local, ...conversations] : conversations
      return { ...state, conversations: merged, nextBefore: action.nextBefore, listError: null }
    }
    case 'listFailed':
      return { ...state, listError: action.error }
    case 'newChat':
      return { ...state, open: null }
    case 'openChat': {
      const summary = state.conversations.find((c) => c.id === action.id)
      return {
        ...state,
        open: {
          id: action.id,
          local: false,
          loading: true,
          loadError: null,
          title: summary?.title ?? '',
          activeLeafId: summary?.active_leaf_id ?? null,
          messages: new Map(),
        },
      }
    }
    case 'loaded': {
      const c = action.conversation
      return withOpen(state, c.id, (open) => ({
        ...open,
        local: false,
        loading: false,
        title: c.title,
        activeLeafId: c.active_leaf_id,
        messages: new Map(c.messages.map((m) => [m.id, m])),
      }))
    }
    case 'loadFailed':
      return withOpen(state, action.id, (open) => ({ ...open, loading: false, loadError: action.error }))
    case 'startLocal': {
      const now = new Date().toISOString()
      const summary: ConversationSummary = {
        id: action.id,
        title: action.title,
        active_leaf_id: action.leafId,
        created_at: now,
        updated_at: now,
      }
      return {
        ...state,
        conversations: [summary, ...state.conversations.filter((c) => c.id !== action.id)],
        open: {
          id: action.id,
          local: true,
          loading: false,
          loadError: null,
          title: action.title,
          activeLeafId: action.leafId,
          messages: new Map(action.messages.map((m) => [m.id, m])),
        },
      }
    }
    case 'upsert':
      return withOpen(state, action.conversationId, (open) => {
        const messages = new Map(open.messages)
        for (const m of action.messages) messages.set(m.id, m)
        return { ...open, messages, activeLeafId: action.leafId === undefined ? open.activeLeafId : action.leafId }
      })
    case 'remove':
      return withOpen(state, action.conversationId, (open) => {
        const messages = new Map(open.messages)
        for (const id of action.ids) messages.delete(id)
        return { ...open, messages }
      })
    case 'setLeaf':
      return withOpen(state, action.conversationId, (open) => ({ ...open, activeLeafId: action.leafId }))
    case 'event':
      return withOpen(state, action.conversationId, (open) => {
        const m = open.messages.get(action.messageId)
        if (!m) return open
        const messages = new Map(open.messages)
        messages.set(m.id, applyEvent(m, action.event))
        return { ...open, messages }
      })
    case 'touched': {
      const rest = state.conversations.filter((c) => c.id !== action.summary.id)
      return {
        ...state,
        conversations: [action.summary, ...rest],
        open:
          state.open?.id === action.summary.id
            ? { ...state.open, local: false, title: action.summary.title }
            : state.open,
      }
    }
    case 'renamed':
      return {
        ...state,
        conversations: state.conversations.map((c) => (c.id === action.id ? { ...c, title: action.title } : c)),
        open: state.open?.id === action.id ? { ...state.open, title: action.title } : state.open,
      }
    case 'deleted':
      return {
        ...state,
        conversations: state.conversations.filter((c) => c.id !== action.id),
        open: state.open?.id === action.id ? null : state.open,
      }
    case 'selectModel':
      return { ...state, modelId: action.id }
  }
}

/** Models on connected clients the user can chat with, refetched on demand. */
function useModels(): [ModelOption[], () => void] {
  const [models, setModels] = useState<ModelOption[]>([])
  const refresh = useCallback(() => {
    listChatModels().then(
      (list) =>
        setModels(
          list.map((m) => ({
            id: `${m.client_id}/${m.model}`,
            target: { kind: 'client', client_id: m.client_id, model: m.model },
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
  conversations: ConversationSummary[]
  hasMore: boolean
  loadMore: () => void
  listError: string | null
  activeId: string | null
  title: string
  loading: boolean
  loadError: string | null
  /** The branch on screen, root first, with each message's siblings. */
  steps: BranchStep[]
  models: ModelOption[]
  /** The model a send would use, or null if there are none. */
  model: ModelOption | null
  selectModel: (id: string) => void
  /** Refetches the model list, e.g. when a picker opens. */
  refreshModels: () => void
  /** A reply in this conversation is being generated: the composer shows Stop. */
  generating: boolean
  /** Stop is possible (the reply exists on the server yet). */
  canStop: boolean
  stop: () => void
  /** Resolves false if the server refused it, so the composer can keep the text. */
  send: (text: string) => Promise<boolean>
  retry: (reply: ChatMessage, target: Target | null) => void
  regenerate: (reply: ChatMessage, target: Target) => void
  edit: (message: ChatMessage, text: string, target: Target) => void
  /** Shows a sibling's branch, down to its newest message. */
  switchTo: (sibling: ChatMessage) => void
  newChat: () => void
  selectChat: (id: string) => void
  renameChat: (id: string, title: string) => Promise<void>
  deleteChat: (id: string) => Promise<void>
  /** The last send/retry/stop that failed, until dismissed. */
  error: string | null
  dismissError: () => void
}

export function useChat(): UseChat {
  const [state, dispatch] = useReducer(reducer, {
    conversations: [],
    nextBefore: null,
    listError: null,
    open: null,
    modelId: null,
  })
  const [models, refreshModels] = useModels()
  const [error, setError] = useState<string | null>(null)
  // Bumped to make the reconnect effect look again after a stream ends.
  const [reconnects, setReconnects] = useState(0)

  // The streams this page is listening to, by reply id. Aborting one only
  // stops listening; the server keeps generating.
  const streams = useRef(new Map<string, AbortController>())
  const stopListening = useCallback(() => {
    for (const c of streams.current.values()) c.abort()
    streams.current.clear()
  }, [])
  useEffect(() => stopListening, [stopListening])

  // For callbacks that need the latest state without being recreated on
  // every streamed chunk.
  const stateRef = useRef(state)
  useLayoutEffect(() => {
    stateRef.current = state
  })

  // ---- the sidebar list ----

  const loadPage = useCallback((before: string | null) => {
    listConversations(before).then(
      (page) =>
        dispatch({ type: 'listLoaded', conversations: page.conversations, nextBefore: page.next_before, append: before !== null }),
      (err: unknown) => dispatch({ type: 'listFailed', error: errorMessage(err) }),
    )
  }, [])
  useEffect(() => loadPage(null), [loadPage])
  const loadMore = useCallback(() => {
    if (stateRef.current.nextBefore) loadPage(stateRef.current.nextBefore)
  }, [loadPage])

  // ---- the open conversation ----

  const openId = state.open?.id ?? null
  const openLocal = state.open?.local ?? false
  const openLoading = state.open?.loading ?? false
  useEffect(() => {
    if (!openId || openLocal || !openLoading) return
    let cancelled = false
    getConversation(openId).then(
      (conversation) => {
        if (!cancelled) dispatch({ type: 'loaded', conversation })
      },
      (err: unknown) => {
        if (cancelled) return
        if (err instanceof ApiError && err.status === 404) {
          // Deleted elsewhere, or a first send that never reached the server.
          dispatch({ type: 'deleted', id: openId })
        } else {
          dispatch({ type: 'loadFailed', id: openId, error: errorMessage(err) })
        }
      },
    )
    return () => {
      cancelled = true
    }
  }, [openId, openLocal, openLoading])

  // Reconnect to any reply in progress that nothing is listening to: on
  // opening a conversation, after a reload, or after a stream broke off.
  useEffect(() => {
    const open = state.open
    if (!open || open.loading) return
    for (const m of open.messages.values()) {
      if (m.role !== 'assistant' || !isActive(m.status) || m.id.startsWith(TEMP) || streams.current.has(m.id)) continue
      const controller = new AbortController()
      streams.current.set(m.id, controller)
      const conversationId = open.id
      streamMessage(m.id, (event) => dispatch({ type: 'event', conversationId, messageId: m.id, event }), controller.signal)
        .catch((err: unknown) => {
          if (!controller.signal.aborted) console.warn(`Reply stream for ${m.id} broke off: ${errorMessage(err)}`)
        })
        .finally(() => {
          if (streams.current.get(m.id) === controller) streams.current.delete(m.id)
          if (!controller.signal.aborted) setTimeout(() => setReconnects((n) => n + 1), RECONNECT_DELAY_MS)
        })
    }
  }, [state.open, reconnects])

  const { byId, steps } = useMemo(() => {
    const messages: ReadonlyMap<string, ChatMessage> = state.open?.messages ?? new Map()
    return { byId: messages, steps: branch(messages, childIndex(messages.values()), state.open?.activeLeafId ?? null) }
  }, [state.open?.messages, state.open?.activeLeafId])

  const activeReply = useMemo(() => {
    for (const m of byId.values()) if (m.role === 'assistant' && isActive(m.status)) return m
    return null
  }, [byId])

  const model = models.find((m) => m.id === state.modelId) ?? models[0] ?? null

  /**
   * Runs one request that creates a reply and streams it. `temps` are the
   * stand-ins shown until the server's `created` event replaces them; if
   * it refuses before that, `undo` puts things back.
   */
  const runReply = useCallback(
    async (
      conversationId: string,
      temps: string[],
      undo: () => void,
      start: (onEvent: (e: ReplyEvent) => void, signal: AbortSignal) => Promise<void>,
    ): Promise<boolean> => {
      const controller = new AbortController()
      let replyId: string | null = null
      const onEvent = (event: ReplyEvent) => {
        if (event.type === 'created') {
          replyId = event.reply.id
          streams.current.set(replyId, controller)
          dispatch({ type: 'remove', conversationId, ids: temps })
          const created = [event.user_message, event.reply].filter((m): m is ChatMessage => !!m)
          dispatch({ type: 'upsert', conversationId, messages: created, leafId: event.reply.id })
          const existing = stateRef.current.conversations.find((c) => c.id === conversationId)
          const summary = event.conversation ?? (existing && { ...existing, updated_at: event.reply.created_at })
          if (summary) dispatch({ type: 'touched', summary: { ...summary, active_leaf_id: event.reply.id } })
        } else if (replyId) {
          dispatch({ type: 'event', conversationId, messageId: replyId, event })
        }
      }
      try {
        await start(onEvent, controller.signal)
        return true
      } catch (err) {
        if (controller.signal.aborted) return true
        if (replyId === null) {
          // Refused before anything was created.
          undo()
          setError(errorMessage(err))
          return false
        }
        // The stream broke off, but the reply carries on: the reconnect
        // effect picks it up.
        console.warn(`Reply stream broke off: ${errorMessage(err)}`)
        return true
      } finally {
        if (replyId && streams.current.get(replyId) === controller) {
          streams.current.delete(replyId)
          if (!controller.signal.aborted) setReconnects((n) => n + 1)
        }
      }
    },
    [],
  )

  const send = useCallback(
    async (text: string): Promise<boolean> => {
      if (!model) return false
      setError(null)
      const open = stateRef.current.open
      const conversationId = open?.id ?? uuid()
      const parent = steps.length > 0 ? steps[steps.length - 1].message : null
      const user = temp(conversationId, 'user', parent?.id ?? null, text)
      const reply = temp(conversationId, 'assistant', user.id, '', model)
      const previousLeaf = open?.activeLeafId ?? null
      if (!open) {
        dispatch({ type: 'startLocal', id: conversationId, title: titleFrom(text), messages: [user, reply], leafId: reply.id })
      } else {
        dispatch({ type: 'upsert', conversationId, messages: [user, reply], leafId: reply.id })
      }
      const undo = () => {
        if (!open) {
          dispatch({ type: 'deleted', id: conversationId })
        } else {
          dispatch({ type: 'remove', conversationId, ids: [user.id, reply.id] })
          dispatch({ type: 'setLeaf', conversationId, leafId: previousLeaf })
        }
      }
      return runReply(conversationId, [user.id, reply.id], undo, (onEvent, signal) =>
        sendMessage(conversationId, { content: text, parent_id: parent?.id ?? null, target: model.target }, onEvent, signal),
      )
    },
    [model, steps, runReply],
  )

  const regenerate = useCallback(
    (replyMessage: ChatMessage, target: Target) => {
      const open = stateRef.current.open
      if (!open || !replyMessage.parent_id) return
      setError(null)
      const option = models.find((m) => m.target.client_id === target.client_id && m.target.model === target.model)
      const reply = temp(open.id, 'assistant', replyMessage.parent_id, '', option)
      const previousLeaf = open.activeLeafId
      dispatch({ type: 'upsert', conversationId: open.id, messages: [reply], leafId: reply.id })
      const undo = () => {
        dispatch({ type: 'remove', conversationId: open.id, ids: [reply.id] })
        dispatch({ type: 'setLeaf', conversationId: open.id, leafId: previousLeaf })
      }
      const parentId = replyMessage.parent_id
      void runReply(open.id, [reply.id], undo, (onEvent, signal) =>
        sendMessage(open.id, { parent_id: parentId, target }, onEvent, signal),
      )
    },
    [models, runReply],
  )

  const edit = useCallback(
    (message: ChatMessage, text: string, target: Target) => {
      const open = stateRef.current.open
      if (!open) return
      setError(null)
      const option = models.find((m) => m.target.client_id === target.client_id && m.target.model === target.model)
      const user = temp(open.id, 'user', message.parent_id, text)
      const reply = temp(open.id, 'assistant', user.id, '', option)
      const previousLeaf = open.activeLeafId
      dispatch({ type: 'upsert', conversationId: open.id, messages: [user, reply], leafId: reply.id })
      const undo = () => {
        dispatch({ type: 'remove', conversationId: open.id, ids: [user.id, reply.id] })
        dispatch({ type: 'setLeaf', conversationId: open.id, leafId: previousLeaf })
      }
      void runReply(open.id, [user.id, reply.id], undo, (onEvent, signal) =>
        sendMessage(open.id, { content: text, parent_id: message.parent_id, target }, onEvent, signal),
      )
    },
    [models, runReply],
  )

  const retry = useCallback(
    (replyMessage: ChatMessage, target: Target | null) => {
      const open = stateRef.current.open
      if (!open) return
      setError(null)
      const option = target && models.find((m) => m.target.client_id === target.client_id && m.target.model === target.model)
      const fresh: ChatMessage = {
        ...replyMessage,
        content: '',
        error: null,
        status: 'queued',
        ...(option && { client_id: option.target.client_id, client_label: option.detail, model: option.target.model }),
      }
      dispatch({ type: 'upsert', conversationId: open.id, messages: [fresh] })
      const undo = () => dispatch({ type: 'upsert', conversationId: open.id, messages: [replyMessage] })
      void runReply(open.id, [], undo, (onEvent, signal) => retryMessage(replyMessage.id, target, onEvent, signal))
    },
    [models, runReply],
  )

  const stop = useCallback(() => {
    const open = stateRef.current.open
    if (!open || !activeReply || activeReply.id.startsWith(TEMP)) return
    cancelMessage(activeReply.id).then(
      (message) => dispatch({ type: 'upsert', conversationId: open.id, messages: [message] }),
      (err: unknown) => setError(`Couldn't stop the reply: ${errorMessage(err)}`),
    )
  }, [activeReply])

  const switchTo = useCallback(
    (sibling: ChatMessage) => {
      const open = stateRef.current.open
      if (!open) return
      const leaf = latestLeaf(childIndex(open.messages.values()), sibling)
      dispatch({ type: 'setLeaf', conversationId: open.id, leafId: leaf.id })
      if (!open.local && !leaf.id.startsWith(TEMP)) {
        updateConversation(open.id, { active_leaf_id: leaf.id }).catch((err: unknown) =>
          console.warn(`Couldn't save the branch: ${errorMessage(err)}`),
        )
      }
    },
    [],
  )

  const newChat = useCallback(() => {
    stopListening()
    setError(null)
    dispatch({ type: 'newChat' })
  }, [stopListening])

  const selectChat = useCallback(
    (id: string) => {
      if (stateRef.current.open?.id === id) return
      stopListening()
      setError(null)
      dispatch({ type: 'openChat', id })
    },
    [stopListening],
  )

  const renameChat = useCallback(async (id: string, title: string) => {
    await updateConversation(id, { title })
    dispatch({ type: 'renamed', id, title: title.trim() })
  }, [])

  const deleteChat = useCallback(
    async (id: string) => {
      await deleteConversation(id)
      if (stateRef.current.open?.id === id) stopListening()
      dispatch({ type: 'deleted', id })
    },
    [stopListening],
  )

  const selectModel = useCallback((id: string) => dispatch({ type: 'selectModel', id }), [])
  const dismissError = useCallback(() => setError(null), [])

  return {
    conversations: state.conversations,
    hasMore: state.nextBefore !== null,
    loadMore,
    listError: state.listError,
    activeId: openId,
    title: state.open?.title || (state.open ? '' : 'New chat'),
    loading: openLoading,
    loadError: state.open?.loadError ?? null,
    steps,
    models,
    model,
    selectModel,
    refreshModels,
    generating: activeReply !== null,
    canStop: activeReply !== null && !activeReply.id.startsWith(TEMP),
    stop,
    send,
    retry,
    regenerate,
    edit,
    switchTo,
    newChat,
    selectChat,
    renameChat,
    deleteChat,
    error,
    dismissError,
  }
}
