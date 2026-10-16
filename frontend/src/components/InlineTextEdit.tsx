import CheckIcon from '@mui/icons-material/Check'
import CloseIcon from '@mui/icons-material/Close'
import EditIcon from '@mui/icons-material/Edit'
import Box from '@mui/material/Box'
import CircularProgress from '@mui/material/CircularProgress'
import IconButton from '@mui/material/IconButton'
import InputAdornment from '@mui/material/InputAdornment'
import TextField from '@mui/material/TextField'
import Tooltip from '@mui/material/Tooltip'
import Typography, { type TypographyProps } from '@mui/material/Typography'
import { useState, type KeyboardEvent } from 'react'
import { errorMessage } from '../api'

interface Props {
  value: string
  /** Rejecting keeps the editor open and shows the error under the field. */
  onSave: (value: string) => Promise<void>
  /** Accessible name for the field and the edit button, e.g. "Client name". */
  label: string
  /** Shown in the empty field; also the hint for what a blank save means. */
  placeholder?: string
  maxLength?: number
  typographyProps?: TypographyProps
}

/**
 * Text that turns into a TextField in place. Enter or the tick saves,
 * Escape or the cross cancels. Blurring does not save, so clicking away
 * never commits a half-typed name.
 */
export default function InlineTextEdit({ value, onSave, label, placeholder, maxLength, typographyProps }: Props) {
  const [draft, setDraft] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const cancel = () => {
    setDraft(null)
    setError(null)
  }

  const save = async () => {
    if (draft === null || saving) return
    if (draft.trim() === value) return cancel()
    setSaving(true)
    setError(null)
    try {
      await onSave(draft)
      setDraft(null)
    } catch (err) {
      setError(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'Enter') {
      e.preventDefault()
      void save()
    } else if (e.key === 'Escape') {
      e.preventDefault()
      // Don't let Escape also close a surrounding dialog.
      e.stopPropagation()
      cancel()
    }
  }

  if (draft === null) {
    return (
      <Box sx={{ display: 'flex', alignItems: 'center', minWidth: 0, '&:hover .inline-edit-button': { opacity: 1 } }}>
        <Typography noWrap {...typographyProps}>
          {value}
        </Typography>
        <Tooltip title={`Edit ${label.toLowerCase()}`}>
          <IconButton
            className="inline-edit-button"
            size="small"
            aria-label={`Edit ${label.toLowerCase()}`}
            onClick={() => setDraft(value)}
            // Faded until hovered, but always visible to keyboard focus and touch.
            sx={{ ml: 0.5, opacity: { xs: 1, sm: 0.4 }, '&:focus-visible': { opacity: 1 } }}
          >
            <EditIcon fontSize="inherit" />
          </IconButton>
        </Tooltip>
      </Box>
    )
  }

  return (
    <TextField
      value={draft}
      onChange={(e) => setDraft(e.target.value)}
      onKeyDown={onKeyDown}
      // Only rendered after the user clicked edit, so focusing is expected.
      // oxlint-disable-next-line jsx-a11y/no-autofocus
      autoFocus
      size="small"
      fullWidth
      disabled={saving}
      placeholder={placeholder}
      error={error !== null}
      helperText={error}
      slotProps={{
        htmlInput: { 'aria-label': label, maxLength },
        input: {
          endAdornment: (
            <InputAdornment position="end">
              {saving ? (
                <CircularProgress size={18} aria-label="Saving" />
              ) : (
                <>
                  <IconButton size="small" aria-label="Save" onClick={() => void save()} edge="end">
                    <CheckIcon fontSize="small" />
                  </IconButton>
                  <IconButton size="small" aria-label="Cancel" onClick={cancel} edge="end">
                    <CloseIcon fontSize="small" />
                  </IconButton>
                </>
              )}
            </InputAdornment>
          ),
        },
      }}
    />
  )
}
