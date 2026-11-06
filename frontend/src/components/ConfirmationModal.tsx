import Alert from '@mui/material/Alert'
import Button, { type ButtonProps } from '@mui/material/Button'
import CircularProgress from '@mui/material/CircularProgress'
import Dialog from '@mui/material/Dialog'
import DialogActions from '@mui/material/DialogActions'
import DialogContent from '@mui/material/DialogContent'
import DialogContentText from '@mui/material/DialogContentText'
import DialogTitle from '@mui/material/DialogTitle'
import { useId, useState, type ReactNode } from 'react'
import { errorMessage } from '../api'

interface Props {
  open: boolean
  title: string
  /** Plain text is wrapped in DialogContentText; pass elements for anything richer. */
  message: ReactNode
  confirmLabel?: string
  cancelLabel?: string
  /** e.g. 'error' for destructive actions. */
  confirmColor?: ButtonProps['color']
  /**
   * May return a promise: the dialog stays open with a spinner until it
   * settles, closes itself on success, and shows the error on failure so
   * the user can retry or cancel.
   */
  onConfirm: () => void | Promise<void>
  onClose: () => void
}

export default function ConfirmationModal({
  open,
  title,
  message,
  confirmLabel = 'Confirm',
  cancelLabel = 'Cancel',
  confirmColor = 'primary',
  onConfirm,
  onClose,
}: Props) {
  const titleId = useId()
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // Every open starts clean, reset during render so a previous error never flashes.
  const [wasOpen, setWasOpen] = useState(open)
  if (open !== wasOpen) {
    setWasOpen(open)
    if (open) {
      setPending(false)
      setError(null)
    }
  }

  const confirm = async () => {
    setPending(true)
    setError(null)
    try {
      await onConfirm()
      onClose()
    } catch (err) {
      setError(errorMessage(err))
    } finally {
      setPending(false)
    }
  }

  // Not dismissable mid-request, so the outcome is never silently lost.
  const close = () => {
    if (!pending) onClose()
  }

  return (
    <Dialog open={open} onClose={close} maxWidth="xs" fullWidth aria-labelledby={titleId}>
      <DialogTitle id={titleId}>{title}</DialogTitle>
      <DialogContent>
        {typeof message === 'string' ? <DialogContentText>{message}</DialogContentText> : message}
        {error && (
          <Alert severity="error" sx={{ mt: 2 }}>
            {error}
          </Alert>
        )}
      </DialogContent>
      <DialogActions>
        <Button onClick={close} disabled={pending}>
          {cancelLabel}
        </Button>
        <Button
          variant="contained"
          color={confirmColor}
          onClick={confirm}
          disabled={pending}
          startIcon={pending ? <CircularProgress size={16} color="inherit" /> : undefined}
        >
          {confirmLabel}
        </Button>
      </DialogActions>
    </Dialog>
  )
}
