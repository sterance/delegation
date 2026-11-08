import { createContext, useContext } from 'react'
import type { ThemeMode } from './theme'

export interface ThemeModeValue {
  mode: ThemeMode
  toggle: () => void
}

/** The light/dark switch, for the app bar and the sign-in pages. */
export const ThemeModeContext = createContext<ThemeModeValue>({ mode: 'light', toggle: () => {} })

export function useThemeModeToggle(): ThemeModeValue {
  return useContext(ThemeModeContext)
}
