import Alert from '@mui/material/Alert'
import Box from '@mui/material/Box'
import Button from '@mui/material/Button'
import Checkbox from '@mui/material/Checkbox'
import FormControlLabel from '@mui/material/FormControlLabel'
import TextField from '@mui/material/TextField'
import Typography from '@mui/material/Typography'
import { useState, type FormEvent } from 'react'
import { Navigate, useNavigate, useSearchParams } from 'react-router'
import { errorMessage, login, signup } from '../api'
import { AuthPending } from '../auth/RequireAuth'
import { safeFrom, useAuth } from '../auth/context'
import AuthLayout from '../components/auth/AuthLayout'
import PasswordField from '../components/common/PasswordField'

type Mode = 'login' | 'signup'

/** Log in and sign up on one page, with a sliding toggle between them. */
export default function Login() {
  const { user, status, signedIn, refreshStatus } = useAuth()
  const navigate = useNavigate()
  const [params] = useSearchParams()
  const from = safeFrom(params.get('from'))

  const [mode, setMode] = useState<Mode>('login')
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [confirm, setConfirm] = useState('')
  const [remember, setRemember] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  if (user === undefined || status === null) return <AuthPending />
  if (status.setup_required) return <Navigate to="/setup" replace />
  if (user) return <Navigate to={from} replace />

  // Sign-up turned off: only "Log in", no toggle.
  const current: Mode = status.signup_enabled ? mode : 'login'

  const switchMode = (next: Mode) => {
    setMode(next)
    setError(null)
    if (next === 'login') setConfirm('')
  }

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setError(null)
    const name = username.trim()
    if (!name) return setError('Enter your username.')
    if (!password) return setError('Enter your password.')
    if (current === 'signup') {
      if ([...password].length < status.password_min_len) {
        return setError(`Passwords must be at least ${status.password_min_len} characters long.`)
      }
      if (password !== confirm) return setError("The passwords don't match.")
    }
    setLoading(true)
    try {
      const me = current === 'login' ? await login(name, password, remember) : await signup(name, password, remember)
      signedIn(me)
      void refreshStatus()
      navigate(from, { replace: true })
    } catch (err) {
      setError(errorMessage(err))
    } finally {
      setLoading(false)
    }
  }

  return (
    <AuthLayout>
      {status.signup_enabled ? (
        <Box
          role="group"
          aria-label="Log in or sign up"
          sx={{
            position: 'relative',
            display: 'flex',
            my: 2,
            border: 1,
            borderColor: 'divider',
            borderRadius: 1,
            overflow: 'hidden',
          }}
        >
          {/* The highlight slides between the two halves. */}
          <Box
            aria-hidden
            sx={{
              position: 'absolute',
              zIndex: 0,
              top: 0,
              bottom: 0,
              width: '50%',
              left: current === 'login' ? 0 : '50%',
              borderRadius: '6px',
              bgcolor: 'action.selected',
              transition: 'left 320ms cubic-bezier(0.34, 1.36, 0.64, 1)',
              '@media (prefers-reduced-motion: reduce)': { transition: 'none' },
            }}
          />
          {(['login', 'signup'] as const).map((m) => (
            <Button
              key={m}
              type="button"
              onClick={() => switchMode(m)}
              aria-pressed={current === m}
              disabled={loading}
              sx={{ position: 'relative', zIndex: 1, flex: 1, borderRadius: 0, color: 'text.primary' }}
            >
              {m === 'login' ? 'Log in' : 'Sign up'}
            </Button>
          ))}
        </Box>
      ) : (
        <Typography variant="h6" component="h1" sx={{ textAlign: 'center', my: 2 }}>
          Log in
        </Typography>
      )}

      {error && (
        <Alert severity="error" sx={{ mb: 2 }} onClose={() => setError(null)}>
          {error}
        </Alert>
      )}
      <form onSubmit={submit} noValidate>
        <TextField
          fullWidth
          label="Username"
          name="username"
          autoComplete="username"
          value={username}
          onChange={(e) => setUsername(e.target.value)}
          margin="normal"
          disabled={loading}
          // oxlint-disable-next-line jsx-a11y/no-autofocus
          autoFocus
          slotProps={{ htmlInput: { autoCapitalize: 'none', spellCheck: false, maxLength: 32 } }}
        />
        <PasswordField
          label="Password"
          name="password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          autoComplete={current === 'login' ? 'current-password' : 'new-password'}
          disabled={loading}
        />
        {current === 'signup' && (
          <PasswordField
            label="Confirm password"
            name="confirm-password"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            autoComplete="new-password"
            disabled={loading}
          />
        )}

        <FormControlLabel
          sx={{ mt: 1 }}
          control={<Checkbox size="small" checked={remember} onChange={(e) => setRemember(e.target.checked)} />}
          label={`Remember me for ${status.remember_days} days`}
        />

        <Button type="submit" variant="contained" fullWidth sx={{ mt: 2 }} disabled={loading}>
          {loading
            ? current === 'login'
              ? 'Signing in…'
              : 'Creating account…'
            : current === 'login'
              ? 'Sign in'
              : 'Create account'}
        </Button>
      </form>
      {current === 'login' && (
        <Typography variant="body2" color="text.secondary" sx={{ mt: 2, textAlign: 'center' }}>
          Forgot your password? Ask the site admin for a reset link.
        </Typography>
      )}
    </AuthLayout>
  )
}
