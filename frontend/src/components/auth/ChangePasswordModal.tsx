import Alert from '@mui/material/Alert'
import Button from '@mui/material/Button'
import CircularProgress from '@mui/material/CircularProgress'
import Dialog from '@mui/material/Dialog'
import DialogActions from '@mui/material/DialogActions'
import DialogContent from '@mui/material/DialogContent'
import DialogTitle from '@mui/material/DialogTitle'
import { useId, useState, type FormEvent } from 'react'
import { changePassword, errorMessage } from '../../api'
import PasswordField from '../common/PasswordField'

interface Props {
  open: boolean
  onClose: () => void
  username: string
  minLength: number
}

export default function ChangePasswordModal({ open, onClose, username, minLength }: Props) {
  const titleId = useId()
  const [current, setCurrent] = useState('')
  const [next, setNext] = useState('')
  const [confirm, setConfirm] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [done, setDone] = useState(false)

  // Every open starts clean, reset during render so nothing old flashes.
  const [wasOpen, setWasOpen] = useState(open)
  if (open !== wasOpen) {
    setWasOpen(open)
    if (open) {
      setCurrent('')
      setNext('')
      setConfirm('')
      setError(null)
      setDone(false)
    }
  }

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setError(null)
    if (!current) return setError('Enter your current password.')
    if ([...next].length < minLength) return setError(`Passwords must be at least ${minLength} characters long.`)
    if (next !== confirm) return setError("The new passwords don't match.")
    setSaving(true)
    try {
      await changePassword(current, next)
      setDone(true)
    } catch (err) {
      setError(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open={open} onClose={() => !saving && onClose()} maxWidth="xs" fullWidth aria-labelledby={titleId}>
      <form onSubmit={submit} noValidate>
        <DialogTitle id={titleId}>Change password</DialogTitle>
        <DialogContent>
          {done ? (
            <Alert severity="success">Password changed. Your other sessions have been signed out.</Alert>
          ) : (
            <>
              {/* Hidden from view but there for password managers. */}
              <input type="text" name="username" autoComplete="username" value={username} readOnly hidden />
              <PasswordField
                label="Current password"
                value={current}
                onChange={(e) => setCurrent(e.target.value)}
                autoComplete="current-password"
                disabled={saving}
                autoFocus
              />
              <PasswordField
                label="New password"
                value={next}
                onChange={(e) => setNext(e.target.value)}
                autoComplete="new-password"
                disabled={saving}
              />
              <PasswordField
                label="Confirm new password"
                value={confirm}
                onChange={(e) => setConfirm(e.target.value)}
                autoComplete="new-password"
                disabled={saving}
              />
              {error && (
                <Alert severity="error" sx={{ mt: 1 }}>
                  {error}
                </Alert>
              )}
            </>
          )}
        </DialogContent>
        <DialogActions>
          {done ? (
            <Button variant="contained" onClick={onClose}>
              Done
            </Button>
          ) : (
            <>
              <Button onClick={onClose} disabled={saving}>
                Cancel
              </Button>
              <Button
                type="submit"
                variant="contained"
                disabled={saving}
                startIcon={saving ? <CircularProgress size={16} color="inherit" /> : undefined}
              >
                Change password
              </Button>
            </>
          )}
        </DialogActions>
      </form>
    </Dialog>
  )
}
