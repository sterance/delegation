import Box from '@mui/material/Box'
import CircularProgress from '@mui/material/CircularProgress'
import Stack from '@mui/material/Stack'
import Typography from '@mui/material/Typography'
import { useLayoutEffect, useRef } from 'react'
import type { ChatMessage } from '../../types/chat'

/** Within this many px of the bottom counts as "at the bottom" for auto-scroll. */
const STICK_THRESHOLD_PX = 48

/** Shared with the composer so both line up in one centred column. */
export const CHAT_COLUMN_MAX_WIDTH = 768

function Message({ message }: { message: ChatMessage }) {
  const isUser = message.role === 'user'
  return (
    <Box sx={{ display: 'flex', justifyContent: isUser ? 'flex-end' : 'flex-start' }}>
      <Box
        sx={{
          maxWidth: isUser ? '80%' : '100%',
          whiteSpace: 'pre-wrap',
          overflowWrap: 'anywhere',
          ...(isUser && { bgcolor: 'action.selected', borderRadius: 3, px: 2, py: 1 }),
        }}
      >
        {message.content && <Typography component="div">{message.content}</Typography>}
        {message.status === 'pending' && !message.content && (
          <CircularProgress size={18} aria-label="Waiting for reply" />
        )}
        {message.status === 'error' && (
          <Typography variant="body2" color="error">
            {message.error ?? 'Something went wrong.'}
          </Typography>
        )}
      </Box>
    </Box>
  )
}

interface Props {
  messages: ChatMessage[]
}

/**
 * Scrollable history. Follows new content to the bottom only while the
 * user is already there, so scrolling up to read isn't yanked away by a
 * new (or, later, streaming) reply. Remount it (via `key`) per
 * conversation to start each one scrolled to the bottom.
 */
export default function MessageList({ messages }: Props) {
  const scrollRef = useRef<HTMLDivElement>(null)
  const atBottom = useRef(true)

  const last = messages[messages.length - 1]
  useLayoutEffect(() => {
    const el = scrollRef.current
    // Your own new message always brings you back down to it.
    if (el && (atBottom.current || last?.role === 'user')) el.scrollTop = el.scrollHeight
  }, [messages, last?.role])

  const handleScroll = () => {
    const el = scrollRef.current
    if (el) atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < STICK_THRESHOLD_PX
  }

  return (
    <Box ref={scrollRef} onScroll={handleScroll} sx={{ flex: 1, minHeight: 0, overflowY: 'auto' }}>
      {messages.length === 0 ? (
        <Box sx={{ height: '100%', display: 'flex', alignItems: 'center', justifyContent: 'center', px: 2 }}>
          <Typography variant="h5" color="text.secondary">
            What can I help with?
          </Typography>
        </Box>
      ) : (
        <Stack spacing={3} sx={{ maxWidth: CHAT_COLUMN_MAX_WIDTH, mx: 'auto', px: 2, py: 3 }}>
          {messages.map((m) => (
            <Message key={m.id} message={m} />
          ))}
        </Stack>
      )}
    </Box>
  )
}
