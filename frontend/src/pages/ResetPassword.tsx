import Alert from '@mui/material/Alert'
import Box from '@mui/material/Box'
import Button from '@mui/material/Button'
import CircularProgress from '@mui/material/CircularProgress'
import TextField from '@mui/material/TextField'
import Typography from '@mui/material/Typography'
import { useEffect, useState, type FormEvent } from 'react'
import { Link, useParams } from 'react-router'
import { errorMessage, getResetInfo, resetPassword } from '../api'
import { useAuth } from '../auth/context'
import AuthLayout from '../components/auth/AuthLayout'
import PasswordField from '../components/common/PasswordField'

type LinkState = { state: 'loading' } | { state: 'ready'; username: string } | { state: 'invalid'; message: string }

/** `/reset/:token`: a single-use link from the admin to set a new password. */
export default function ResetPassword() {
  const { token = '' } = useParams()
  const { status } = useAuth()
  const [link, setLink] = useState<LinkState>({ state: 'loading' })
  const [password, setPassword] = useState('')
  const [confirm, setConfirm] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [done, setDone] = useState(false)

  useEffect(() => {
    let cancelled = false
    getResetInfo(token).then(
      (info) => {
        if (!cancelled) setLink({ state: 'ready', username: info.username })
      },
      (err: unknown) => {
        if (!cancelled) setLink({ state: 'invalid', message: errorMessage(err) })
      },
    )
    return () => {
      cancelled = true
    }
  }, [token])

  const minLen = status?.password_min_len ?? 12

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setError(null)
    if ([...password].length < minLen) return setError(`Passwords must be at least ${minLen} characters long.`)
    if (password !== confirm) return setError("The passwords don't match.")
    setSaving(true)
    try {
      await resetPassword(token, password)
      setDone(true)
    } catch (err) {
      setError(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <AuthLayout>
      <Typography variant="h6" component="h1" sx={{ textAlign: 'center', mt: 2, mb: 2 }}>
        Set a new password
      </Typography>
      {link.state === 'loading' && (
        <Box sx={{ display: 'flex', justifyContent: 'center', py: 4 }}>
          <CircularProgress aria-label="Checking the link" />
        </Box>
      )}
      {link.state === 'invalid' && (
        <>
          <Alert severity="error">{link.message}</Alert>
          <Button component={Link} to="/login" fullWidth sx={{ mt: 2 }}>
            Go to sign in
          </Button>
        </>
      )}
      {link.state === 'ready' && done && (
        <>
          <Alert severity="success">
            Your password has been changed and you've been signed out everywhere. Sign in with the new password.
          </Alert>
          <Button component={Link} to="/login" variant="contained" fullWidth sx={{ mt: 2 }}>
            Sign in
          </Button>
        </>
      )}
      {link.state === 'ready' && !done && (
        <>
          {error && (
            <Alert severity="error" sx={{ mb: 2 }} onClose={() => setError(null)}>
              {error}
            </Alert>
          )}
          <form onSubmit={submit} noValidate>
            {/* Read-only, so password managers know whose password this is. */}
            <TextField
              fullWidth
              label="Account"
              name="username"
              autoComplete="username"
              value={link.username}
              margin="normal"
              slotProps={{ htmlInput: { readOnly: true } }}
            />
            <PasswordField
              label="New password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete="new-password"
              disabled={saving}
              autoFocus
            />
            <PasswordField
              label="Confirm new password"
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
              autoComplete="new-password"
              disabled={saving}
            />
            <Button type="submit" variant="contained" fullWidth sx={{ mt: 2 }} disabled={saving}>
              {saving ? 'Saving…' : 'Set password'}
            </Button>
          </form>
        </>
      )}
    </AuthLayout>
  )
}
