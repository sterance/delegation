import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react'
import { errorMessage, getAuthStatus, getMe, logout, setUnauthorizedHandler, type AuthStatus, type User } from '../api'
import { AuthContext, type AuthValue } from './context'

/** Who's signed in, and the site's sign-in settings, for the whole app. */
export default function AuthProvider({ children }: { children: ReactNode }) {
  const [user, setUser] = useState<User | null | undefined>(undefined)
  const [status, setStatus] = useState<AuthStatus | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [attempt, setAttempt] = useState(0)

  useEffect(() => {
    let cancelled = false
    Promise.all([getAuthStatus(), getMe()]).then(
      ([status, me]) => {
        if (cancelled) return
        setStatus(status)
        setUser(me)
        setLoadError(null)
      },
      (err: unknown) => {
        if (!cancelled) setLoadError(errorMessage(err))
      },
    )
    return () => {
      cancelled = true
    }
  }, [attempt])

  // Any 401 from the API: the session ended (expired, signed out
  // elsewhere, disabled). The route guard then shows the login page.
  useEffect(() => {
    setUnauthorizedHandler(() => setUser(null))
    return () => setUnauthorizedHandler(null)
  }, [])

  const refreshStatus = useCallback(async () => {
    setStatus(await getAuthStatus())
  }, [])

  const signOut = useCallback(async () => {
    try {
      await logout()
    } finally {
      setUser(null)
    }
  }, [])

  const value = useMemo<AuthValue>(
    () => ({
      user,
      status,
      loadError,
      retry: () => {
        setLoadError(null)
        setAttempt((n) => n + 1)
      },
      signedIn: setUser,
      refreshStatus,
      signOut,
    }),
    [user, status, loadError, refreshStatus, signOut],
  )

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>
}
