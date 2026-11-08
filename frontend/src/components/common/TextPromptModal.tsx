import Alert from '@mui/material/Alert'
import Button from '@mui/material/Button'
import CircularProgress from '@mui/material/CircularProgress'
import Dialog from '@mui/material/Dialog'
import DialogActions from '@mui/material/DialogActions'
import DialogContent from '@mui/material/DialogContent'
import DialogTitle from '@mui/material/DialogTitle'
import TextField from '@mui/material/TextField'
import { useId, useState, type FormEvent } from 'react'
import { errorMessage } from '../../api'

interface Props {
  open: boolean
  title: string
  label: string
  initialValue: string
  maxLength?: number
  confirmLabel?: string
  /** Rejecting keeps the dialog open and shows the error. */
  onConfirm: (value: string) => Promise<void>
  onClose: () => void
}

/** A dialog asking for one line of text, e.g. a new name. */
export default function TextPromptModal({
  open,
  title,
  label,
  initialValue,
  maxLength,
  confirmLabel = 'Save',
  onConfirm,
  onClose,
}: Props) {
  const titleId = useId()
  const [value, setValue] = useState(initialValue)
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // Every open starts from the current value, reset during render so the
  // previous one never flashes.
  const [wasOpen, setWasOpen] = useState(open)
  if (open !== wasOpen) {
    setWasOpen(open)
    if (open) {
      setValue(initialValue)
      setPending(false)
      setError(null)
    }
  }

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    if (!value.trim() || pending) return
    setPending(true)
    setError(null)
    try {
      await onConfirm(value.trim())
      onClose()
    } catch (err) {
      setError(errorMessage(err))
    } finally {
      setPending(false)
    }
  }

  return (
    <Dialog open={open} onClose={() => !pending && onClose()} maxWidth="xs" fullWidth aria-labelledby={titleId}>
      <form onSubmit={submit}>
        <DialogTitle id={titleId}>{title}</DialogTitle>
        <DialogContent>
          <TextField
            value={value}
            onChange={(e) => setValue(e.target.value)}
            label={label}
            fullWidth
            // oxlint-disable-next-line jsx-a11y/no-autofocus
            autoFocus
            margin="dense"
            disabled={pending}
            slotProps={{ htmlInput: { maxLength } }}
          />
          {error && (
            <Alert severity="error" sx={{ mt: 2 }}>
              {error}
            </Alert>
          )}
        </DialogContent>
        <DialogActions>
          <Button onClick={onClose} disabled={pending}>
            Cancel
          </Button>
          <Button
            type="submit"
            variant="contained"
            disabled={pending || !value.trim()}
            startIcon={pending ? <CircularProgress size={16} color="inherit" /> : undefined}
          >
            {confirmLabel}
          </Button>
        </DialogActions>
      </form>
    </Dialog>
  )
}
