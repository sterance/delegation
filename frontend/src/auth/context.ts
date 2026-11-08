import { createContext, useContext } from 'react'
import type { AuthStatus, User } from '../api'

export interface AuthValue {
  /** Undefined while the first check is running; null when signed out. */
  user: User | null | undefined
  /** What the login page should offer. Null until loaded (or if that failed). */
  status: AuthStatus | null
  /** Set if the server couldn't be reached at all. */
  loadError: string | null
  retry: () => void
  /** After a successful login, sign-up, setup or reset. */
  signedIn: (user: User) => void
  refreshStatus: () => Promise<void>
  signOut: () => Promise<void>
}

export const AuthContext = createContext<AuthValue | null>(null)

export function useAuth(): AuthValue {
  const value = useContext(AuthContext)
  if (!value) throw new Error('useAuth outside AuthProvider')
  return value
}

/** Only same-site paths, so `?from=` can't send anyone elsewhere after login. */
export function safeFrom(raw: string | null): string {
  if (!raw || !raw.startsWith('/') || raw.startsWith('//') || raw.startsWith('/\\')) return '/clients'
  if (raw.startsWith('/login') || raw.startsWith('/setup') || raw.startsWith('/reset')) return '/clients'
  return raw
}
