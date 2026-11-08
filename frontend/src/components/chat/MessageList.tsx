import ArrowDropDownIcon from '@mui/icons-material/ArrowDropDown'
import EditOutlinedIcon from '@mui/icons-material/EditOutlined'
import RefreshIcon from '@mui/icons-material/Refresh'
import Alert from '@mui/material/Alert'
import Box from '@mui/material/Box'
import Button from '@mui/material/Button'
import ButtonGroup from '@mui/material/ButtonGroup'
import CircularProgress from '@mui/material/CircularProgress'
import IconButton from '@mui/material/IconButton'
import Stack from '@mui/material/Stack'
import TextField from '@mui/material/TextField'
import Tooltip from '@mui/material/Tooltip'
import Typography from '@mui/material/Typography'
import { useLayoutEffect, useRef, useState, type KeyboardEvent } from 'react'
import { isActive } from '../../api'
import type { ChatMessage, ModelOption, Target } from '../../types/chat'
import type { BranchStep } from '../../utils/tree'
import BranchSwitcher from './BranchSwitcher'
import ModelMenu from './ModelMenu'

/** Within this many px of the bottom counts as "at the bottom" for auto-scroll. */
const STICK_THRESHOLD_PX = 48

/** Shared with the composer so both line up in one centred column. */
export const CHAT_COLUMN_MAX_WIDTH = 768

export interface MessageActions {
  models: ModelOption[]
  /** The composer's model: what Regenerate and Edit use unless another is picked. */
  model: ModelOption | null
  /** A reply is being generated: nothing else can start one. */
  busy: boolean
  onRetry: (reply: ChatMessage, target: Target | null) => void
  onRegenerate: (reply: ChatMessage, target: Target) => void
  onEdit: (message: ChatMessage, text: string, target: Target) => void
  onSwitch: (sibling: ChatMessage) => void
  /** Refreshes the model list before a menu opens. */
  onOpenModels: () => void
}

/** The id of the option that answered this reply, if it's still available. */
function optionFor(models: ModelOption[], m: ChatMessage): string | null {
  return models.find((o) => o.target.client_id === m.client_id && o.target.model === m.model)?.id ?? null
}

function UserMessage({ step, actions }: { step: BranchStep; actions: MessageActions }) {
  const { message } = step
  const [draft, setDraft] = useState<string | null>(null)
  const canSave = draft !== null && draft.trim().length > 0 && !actions.busy && actions.model !== null

  const save = () => {
    if (!canSave || !actions.model) return
    actions.onEdit(message, draft.trim(), actions.model.target)
    setDraft(null)
  }
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault()
      save()
    } else if (e.key === 'Escape') {
      setDraft(null)
    }
  }

  if (draft !== null) {
    return (
      <Stack spacing={1} sx={{ alignItems: 'flex-end' }}>
        <TextField
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={onKeyDown}
          multiline
          fullWidth
          maxRows={15}
          // Only shown after the user clicked Edit.
          // oxlint-disable-next-line jsx-a11y/no-autofocus
          autoFocus
          slotProps={{ htmlInput: { 'aria-label': 'Edit message' } }}
        />
        <Stack direction="row" spacing={1}>
          <Button onClick={() => setDraft(null)}>Cancel</Button>
          <Tooltip title={actions.busy ? 'Wait for the reply in progress' : ''}>
            <span>
              <Button variant="contained" onClick={save} disabled={!canSave}>
                Send
              </Button>
            </span>
          </Tooltip>
        </Stack>
      </Stack>
    )
  }

  return (
    <Stack sx={{ alignItems: 'flex-end', '&:hover .message-actions': { opacity: 1 } }}>
      <Box
        sx={{
          maxWidth: '80%',
          whiteSpace: 'pre-wrap',
          overflowWrap: 'anywhere',
          bgcolor: 'action.selected',
          borderRadius: 3,
          px: 2,
          py: 1,
        }}
      >
        <Typography component="div">{message.content}</Typography>
      </Box>
      <Stack
        direction="row"
        className="message-actions"
        sx={{ alignItems: 'center', mt: 0.25, opacity: { xs: 1, md: step.siblings.length > 1 ? 1 : 0 } }}
      >
        <BranchSwitcher position={step.position} siblings={step.siblings} onSwitch={actions.onSwitch} />
        <Tooltip title="Edit">
          <IconButton size="small" aria-label="Edit message" onClick={() => setDraft(message.content)}>
            <EditOutlinedIcon fontSize="small" />
          </IconButton>
        </Tooltip>
      </Stack>
    </Stack>
  )
}

function StatusNote({ message }: { message: ChatMessage }) {
  switch (message.status) {
    case 'failed':
      return (
        <Typography variant="body2" color="error">
          {message.error ?? 'The reply failed.'}
        </Typography>
      )
    case 'orphaned':
      return (
        <Typography variant="body2" color="warning.main">
          {/* "Interrupted by a server restart", or "Interrupted. The client disconnected…" */}
          {message.error?.startsWith('Interrupted')
            ? `${message.error}.`
            : `Interrupted.${message.error ? ` ${message.error}` : ''}`}
        </Typography>
      )
    case 'cancelled':
      return (
        <Typography variant="body2" color="text.secondary">
          Stopped.
        </Typography>
      )
    default:
      return null
  }
}

function AssistantMessage({ step, actions }: { step: BranchStep; actions: MessageActions }) {
  const { message } = step
  const [menu, setMenu] = useState<{ anchor: HTMLElement; kind: 'retry' | 'regenerate' } | null>(null)
  const active = isActive(message.status)
  const retryable = message.status === 'failed' || message.status === 'orphaned'
  const answeredBy = [message.model, message.client_label].filter(Boolean).join(' · ')
  const openMenu = (anchor: HTMLElement, kind: 'retry' | 'regenerate') => {
    actions.onOpenModels()
    setMenu({ anchor, kind })
  }

  return (
    <Stack sx={{ alignItems: 'flex-start', '&:hover .message-actions': { opacity: 1 } }}>
      <Box sx={{ maxWidth: '100%', whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' }}>
        {message.content && <Typography component="div">{message.content}</Typography>}
        {active && !message.content && (
          <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
            <CircularProgress size={16} aria-label="Waiting for the reply" />
            <Typography variant="body2" color="text.secondary">
              {message.status === 'queued' ? 'Starting…' : `Waiting for ${message.client_label ?? 'the client'}…`}
            </Typography>
          </Stack>
        )}
        <StatusNote message={message} />
      </Box>

      {retryable && (
        <ButtonGroup size="small" variant="outlined" sx={{ mt: 1 }} disabled={actions.busy}>
          <Button startIcon={<RefreshIcon />} onClick={() => actions.onRetry(message, null)}>
            Retry
          </Button>
          <Button
            aria-label="Retry with another model"
            aria-haspopup="menu"
            onClick={(e) => openMenu(e.currentTarget, 'retry')}
          >
            <ArrowDropDownIcon fontSize="small" />
          </Button>
        </ButtonGroup>
      )}

      <Stack
        direction="row"
        className="message-actions"
        sx={{
          alignItems: 'center',
          mt: 0.25,
          minHeight: 30,
          opacity: { xs: 1, md: active || step.siblings.length > 1 ? 1 : 0.6 },
        }}
      >
        <BranchSwitcher position={step.position} siblings={step.siblings} onSwitch={actions.onSwitch} />
        {!active && message.parent_id && (
          <Tooltip title={actions.busy ? 'Wait for the reply in progress' : 'Regenerate'}>
            <span>
              <IconButton
                size="small"
                aria-label="Regenerate"
                aria-haspopup="menu"
                disabled={actions.busy}
                onClick={(e) => openMenu(e.currentTarget, 'regenerate')}
              >
                <RefreshIcon fontSize="small" />
              </IconButton>
            </span>
          </Tooltip>
        )}
        {answeredBy && (
          <Typography variant="caption" color="text.secondary" sx={{ ml: 0.5 }}>
            {answeredBy}
            {message.attempts > 1 ? ` · attempt ${message.attempts}` : ''}
          </Typography>
        )}
      </Stack>

      <ModelMenu
        anchor={menu?.anchor ?? null}
        onClose={() => setMenu(null)}
        models={actions.models}
        heading={menu?.kind === 'retry' ? 'Retry with' : 'Regenerate with'}
        selectedId={menu?.kind === 'retry' ? optionFor(actions.models, message) : (actions.model?.id ?? null)}
        onPick={(m) => {
          if (menu?.kind === 'retry') actions.onRetry(message, m.target)
          else actions.onRegenerate(message, m.target)
        }}
      />
    </Stack>
  )
}

interface Props extends MessageActions {
  steps: BranchStep[]
  loading?: boolean
  loadError?: string | null
}

/**
 * The branch on screen. Follows new content to the bottom only while the
 * user is already there, so scrolling up to read isn't yanked away by a
 * streaming reply. Remount it (via `key`) per conversation to start each
 * one scrolled to the bottom.
 */
export default function MessageList({ steps, loading = false, loadError = null, ...actions }: Props) {
  const scrollRef = useRef<HTMLDivElement>(null)
  const atBottom = useRef(true)

  const last = steps[steps.length - 1]?.message
  useLayoutEffect(() => {
    const el = scrollRef.current
    // Your own new message always brings you back down to it.
    if (el && (atBottom.current || last?.role === 'user' || last?.id.startsWith('tmp-'))) {
      el.scrollTop = el.scrollHeight
    }
  }, [steps, last?.role, last?.id, last?.content])

  const handleScroll = () => {
    const el = scrollRef.current
    if (el) atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < STICK_THRESHOLD_PX
  }

  let body
  if (loadError) {
    body = (
      <Box sx={{ maxWidth: CHAT_COLUMN_MAX_WIDTH, mx: 'auto', p: 2 }}>
        <Alert severity="error">Couldn't load this chat: {loadError}</Alert>
      </Box>
    )
  } else if (loading) {
    body = (
      <Box sx={{ height: '100%', display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
        <CircularProgress aria-label="Loading chat" />
      </Box>
    )
  } else if (steps.length === 0) {
    body = (
      <Box sx={{ height: '100%', display: 'flex', alignItems: 'center', justifyContent: 'center', px: 2 }}>
        <Typography variant="h5" color="text.secondary">
          What can I help with?
        </Typography>
      </Box>
    )
  } else {
    body = (
      <Stack spacing={2.5} sx={{ maxWidth: CHAT_COLUMN_MAX_WIDTH, mx: 'auto', px: 2, py: 3 }}>
        {steps.map((step) =>
          step.message.role === 'user' ? (
            <UserMessage key={step.message.id} step={step} actions={actions} />
          ) : (
            <AssistantMessage key={step.message.id} step={step} actions={actions} />
          ),
        )}
      </Stack>
    )
  }

  return (
    <Box ref={scrollRef} onScroll={handleScroll} sx={{ flex: 1, minHeight: 0, overflowY: 'auto' }}>
      {body}
    </Box>
  )
}
