import SendIcon from '@mui/icons-material/Send'
import StopCircleIcon from '@mui/icons-material/StopCircle'
import IconButton from '@mui/material/IconButton'
import InputBase from '@mui/material/InputBase'
import Paper from '@mui/material/Paper'
import Stack from '@mui/material/Stack'
import Tooltip from '@mui/material/Tooltip'
import { useState, type KeyboardEvent } from 'react'
import type { ModelOption } from '../../types/chat'
import ModelPicker from './ModelPicker'

/** The box grows with its text up to this many lines, then scrolls. */
const MAX_LINES = 15

interface Props {
  /** Resolves false if the message wasn't accepted, to put the text back. */
  onSend: (text: string) => Promise<boolean> | boolean
  /** Blocks sending (e.g. no model); typing is still allowed. */
  disabled?: boolean
  /** A reply is being generated: Stop replaces Send. */
  generating?: boolean
  canStop?: boolean
  onStop?: () => void
  models: ModelOption[]
  modelId: string | null
  onSelectModel: (id: string) => void
  onOpenModels?: () => void
}

export default function Composer({
  onSend,
  disabled = false,
  generating = false,
  canStop = false,
  onStop,
  models,
  modelId,
  onSelectModel,
  onOpenModels,
}: Props) {
  const [text, setText] = useState('')
  const canSend = !disabled && !generating && text.trim().length > 0

  const submit = async () => {
    if (!canSend) return
    const sent = text.trim()
    setText('')
    if (!(await onSend(sent))) setText((current) => current || sent)
  }

  // Enter sends, Shift+Enter is a newline. Ignored mid-IME composition so
  // confirming a CJK candidate doesn't send.
  const handleKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault()
      void submit()
    }
  }

  return (
    <Paper variant="outlined" sx={{ borderRadius: 3, px: 2, pt: 1.5, pb: 1 }}>
      <InputBase
        multiline
        fullWidth
        autoFocus
        minRows={1}
        maxRows={MAX_LINES}
        placeholder="Message…"
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={handleKeyDown}
        inputProps={{ 'aria-label': 'Message' }}
        sx={{ p: 0 }}
      />
      <Stack direction="row" sx={{ alignItems: 'center', justifyContent: 'space-between', mt: 1, ml: -1, minWidth: 0 }}>
        <ModelPicker models={models} selectedId={modelId} onSelect={onSelectModel} onOpen={onOpenModels} />
        {generating ? (
          <Tooltip title="Stop generating">
            {/* span: Tooltip needs an element that fires events even when the button is disabled. */}
            <span>
              <IconButton color="primary" onClick={onStop} disabled={!canStop} aria-label="Stop generating">
                <StopCircleIcon />
              </IconButton>
            </span>
          </Tooltip>
        ) : (
          <Tooltip title="Send (Enter)">
            <span>
              <IconButton color="primary" onClick={() => void submit()} disabled={!canSend} aria-label="Send message">
                <SendIcon />
              </IconButton>
            </span>
          </Tooltip>
        )}
      </Stack>
    </Paper>
  )
}
