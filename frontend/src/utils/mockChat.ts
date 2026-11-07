// Layout-only sample data so the chat page has something to show before it's
// wired up. Delete this file (and its import in hooks/useChat.ts) once real
// conversations exist.

import type { Conversation, ModelOption } from '../types/chat'

export const MOCK_MODELS: ModelOption[] = [
  { id: 'placeholder-small', label: 'Placeholder small', detail: 'Not connected yet' },
  { id: 'placeholder-large', label: 'Placeholder large', detail: 'Not connected yet' },
]

const at = (minutesAgo: number) => new Date(Date.now() - minutesAgo * 60_000).toISOString()

const LONG_REPLY = Array.from(
  { length: 6 },
  (_, i) =>
    `Paragraph ${i + 1}. This is filler text to check how long assistant replies wrap and scroll. ` +
    'It should flow at a comfortable reading width, keep blank lines between paragraphs, ' +
    'and push older messages up out of view without moving the reply box.',
).join('\n\n')

export const MOCK_CONVERSATIONS: Conversation[] = [
  {
    id: 'mock-1',
    title: 'Layout test',
    updatedAt: at(5),
    messages: [
      { id: 'm1', role: 'user', content: 'Hello! Is this thing on?', createdAt: at(7), status: 'done' },
      {
        id: 'm2',
        role: 'assistant',
        content: 'Loud and clear. This is a placeholder reply.',
        createdAt: at(7),
        status: 'done',
      },
      {
        id: 'm3',
        role: 'user',
        content: 'Give me something long so I can check scrolling.\nAlso, a second line.',
        createdAt: at(6),
        status: 'done',
      },
      { id: 'm4', role: 'assistant', content: LONG_REPLY, createdAt: at(5), status: 'done' },
    ],
  },
  {
    id: 'mock-2',
    title: 'A conversation with a much longer title that should get truncated in the sidebar',
    updatedAt: at(60 * 26),
    messages: [
      { id: 'm5', role: 'user', content: 'Short one.', createdAt: at(60 * 26), status: 'done' },
      {
        id: 'm6',
        role: 'assistant',
        content: '',
        createdAt: at(60 * 26),
        status: 'error',
        error: 'Example of a failed reply.',
      },
    ],
  },
  { id: 'mock-3', title: 'Empty chat', updatedAt: at(60 * 24 * 8), messages: [] },
]
