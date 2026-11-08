import Alert from '@mui/material/Alert'
import Box from '@mui/material/Box'
import Button from '@mui/material/Button'
import CircularProgress from '@mui/material/CircularProgress'
import type { ReactNode } from 'react'
import { Navigate, useLocation } from 'react-router'
import { useAuth } from './context'

/** While the first auth check runs, or if the server can't be reached. */
export function AuthPending() {
  const { loadError, retry } = useAuth()
  return (
    <Box sx={{ display: 'flex', justifyContent: 'center', alignItems: 'center', minHeight: '60dvh', px: 2 }}>
      {loadError ? (
        <Alert
          severity="error"
          action={
            <Button color="inherit" size="small" onClick={retry}>
              Retry
            </Button>
          }
        >
          Couldn't reach the server: {loadError}
        </Alert>
      ) : (
        <CircularProgress aria-label="Loading" />
      )}
    </Box>
  )
}

/**
 * Shows `children` only to a signed-in user (the admin, with `admin`).
 * Anyone else goes to /login, and comes back here afterwards.
 */
export default function RequireAuth({ children, admin = false }: { children: ReactNode; admin?: boolean }) {
  const { user, status } = useAuth()
  const location = useLocation()
  if (user === undefined || status === null) return <AuthPending />
  if (status.setup_required) return <Navigate to="/setup" replace />
  if (!user) {
    const from = encodeURIComponent(location.pathname + location.search)
    return <Navigate to={`/login?from=${from}`} replace />
  }
  if (admin && user.role !== 'admin') return <Navigate to="/" replace />
  return children
}
