import Alert from '@mui/material/Alert'
import Button from '@mui/material/Button'
import Checkbox from '@mui/material/Checkbox'
import FormControlLabel from '@mui/material/FormControlLabel'
import TextField from '@mui/material/TextField'
import Typography from '@mui/material/Typography'
import { useState, type FormEvent } from 'react'
import { Navigate, useNavigate } from 'react-router'
import { errorMessage, setup } from '../api'
import { AuthPending } from '../auth/RequireAuth'
import { useAuth } from '../auth/context'
import AuthLayout from '../components/auth/AuthLayout'
import PasswordField from '../components/common/PasswordField'

/** First run: creates the admin account, unlocked by the token in the server log. */
export default function Setup() {
  const { user, status, signedIn, refreshStatus } = useAuth()
  const navigate = useNavigate()
  const [token, setToken] = useState('')
  const [username, setUsername] = useState('admin')
  const [password, setPassword] = useState('')
  const [confirm, setConfirm] = useState('')
  const [remember, setRemember] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  if (user === undefined || status === null) return <AuthPending />
  if (!status.setup_required) return <Navigate to={user ? '/' : '/login'} replace />

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setError(null)
    if (!token.trim()) return setError('Enter the setup token from the server log.')
    if (!username.trim()) return setError('Choose a username.')
    if ([...password].length < status.password_min_len) {
      return setError(`Passwords must be at least ${status.password_min_len} characters long.`)
    }
    if (password !== confirm) return setError("The passwords don't match.")
    setLoading(true)
    try {
      const me = await setup(token.trim(), username.trim(), password, remember)
      signedIn(me)
      await refreshStatus()
      navigate('/clients', { replace: true })
    } catch (err) {
      setError(errorMessage(err))
    } finally {
      setLoading(false)
    }
  }

  return (
    <AuthLayout>
      <Typography variant="h6" component="h1" sx={{ textAlign: 'center', mt: 2 }}>
        Set up Delegation
      </Typography>
      <Typography variant="body2" color="text.secondary" sx={{ textAlign: 'center', mt: 1, mb: 2 }}>
        Create the admin account. The setup token is printed in the server log when it starts without an admin.
      </Typography>
      {error && (
        <Alert severity="error" sx={{ mb: 2 }} onClose={() => setError(null)}>
          {error}
        </Alert>
      )}
      <form onSubmit={submit} noValidate>
        <TextField
          fullWidth
          label="Setup token"
          value={token}
          onChange={(e) => setToken(e.target.value)}
          margin="normal"
          autoComplete="one-time-code"
          disabled={loading}
          // oxlint-disable-next-line jsx-a11y/no-autofocus
          autoFocus
          slotProps={{ htmlInput: { autoCapitalize: 'none', spellCheck: false } }}
        />
        <TextField
          fullWidth
          label="Admin username"
          name="username"
          autoComplete="username"
          value={username}
          onChange={(e) => setUsername(e.target.value)}
          margin="normal"
          disabled={loading}
          slotProps={{ htmlInput: { autoCapitalize: 'none', spellCheck: false, maxLength: 32 } }}
        />
        <PasswordField
          label="Password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          autoComplete="new-password"
          disabled={loading}
        />
        <PasswordField
          label="Confirm password"
          value={confirm}
          onChange={(e) => setConfirm(e.target.value)}
          autoComplete="new-password"
          disabled={loading}
        />
        <FormControlLabel
          sx={{ mt: 1 }}
          control={<Checkbox size="small" checked={remember} onChange={(e) => setRemember(e.target.checked)} />}
          label={`Remember me for ${status.remember_days} days`}
        />
        <Button type="submit" variant="contained" fullWidth sx={{ mt: 2 }} disabled={loading}>
          {loading ? 'Creating account…' : 'Create admin account'}
        </Button>
      </form>
    </AuthLayout>
  )
}
