import SendIcon from '@mui/icons-material/Send'
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
  onSend: (text: string) => void
  /** Blocks sending (e.g. while a reply is pending); typing is still allowed. */
  disabled?: boolean
  models: ModelOption[]
  modelId: string | null
  onSelectModel: (id: string) => void
  onOpenModels?: () => void
}

export default function Composer({ onSend, disabled = false, models, modelId, onSelectModel, onOpenModels }: Props) {
  const [text, setText] = useState('')
  const canSend = !disabled && text.trim().length > 0

  const submit = () => {
    if (!canSend) return
    onSend(text.trim())
    setText('')
  }

  // Enter sends, Shift+Enter is a newline. Ignored mid-IME composition so
  // confirming a CJK candidate doesn't send.
  const handleKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault()
      submit()
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
      <Stack direction="row" sx={{ alignItems: 'center', justifyContent: 'space-between', mt: 1, ml: -1 }}>
        <ModelPicker models={models} selectedId={modelId} onSelect={onSelectModel} onOpen={onOpenModels} />
        <Tooltip title="Send (Enter)">
          {/* span: Tooltip needs an element that fires events even when the button is disabled. */}
          <span>
            <IconButton color="primary" onClick={submit} disabled={!canSend} aria-label="Send message">
              <SendIcon />
            </IconButton>
          </span>
        </Tooltip>
      </Stack>
    </Paper>
  )
}
