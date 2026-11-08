// A conversation is a tree of messages (each points at its parent). These
// work out the branch on screen and where the sibling switchers go.

import type { ChatMessage } from '../types/chat'

/** Children of each message (`''` for the roots), oldest first. */
export function childIndex(messages: Iterable<ChatMessage>): Map<string, ChatMessage[]> {
  const index = new Map<string, ChatMessage[]>()
  for (const m of messages) {
    const key = m.parent_id ?? ''
    const list = index.get(key)
    if (list) list.push(m)
    else index.set(key, [m])
  }
  for (const list of index.values()) list.sort((a, b) => a.created_at.localeCompare(b.created_at))
  return index
}

/** The newest leaf under `start` (itself if it has no children). */
export function latestLeaf(index: Map<string, ChatMessage[]>, start: ChatMessage): ChatMessage {
  let best = start
  const stack = [start]
  while (stack.length > 0) {
    const m = stack.pop()!
    const children = index.get(m.id)
    if (!children || children.length === 0) {
      if (m.created_at.localeCompare(best.created_at) > 0 || best === start) best = m
    } else {
      stack.push(...children)
    }
  }
  return best
}

export interface BranchStep {
  message: ChatMessage
  /** 1-based position among its siblings. */
  position: number
  siblings: ChatMessage[]
}

/**
 * The branch ending at `leafId`, root first. If that message is missing,
 * falls back to the newest leaf under the newest root.
 */
export function branch(
  byId: ReadonlyMap<string, ChatMessage>,
  index: Map<string, ChatMessage[]>,
  leafId: string | null,
): BranchStep[] {
  let leaf = leafId ? byId.get(leafId) : undefined
  if (!leaf) {
    const roots = index.get('') ?? []
    const root = roots[roots.length - 1]
    if (!root) return []
    leaf = latestLeaf(index, root)
  }
  const path: ChatMessage[] = []
  const seen = new Set<string>()
  for (let m: ChatMessage | undefined = leaf; m && !seen.has(m.id); m = m.parent_id ? byId.get(m.parent_id) : undefined) {
    seen.add(m.id)
    path.push(m)
  }
  path.reverse()
  return path.map((message) => {
    const siblings = index.get(message.parent_id ?? '') ?? [message]
    return { message, position: siblings.indexOf(message) + 1, siblings }
  })
}
