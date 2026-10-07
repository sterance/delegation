import { createTheme } from '@mui/material/styles'

export type ThemeMode = 'light' | 'dark'

// Only ever written once the user explicitly toggles the theme. While it's
// absent, the app follows the browser/OS preference live.
const STORAGE_KEY = 'theme-preference'

export function readStoredMode(): ThemeMode | null {
  try {
    const value = localStorage.getItem(STORAGE_KEY)
    return value === 'light' || value === 'dark' ? value : null
  } catch {
    // Storage can be unavailable (privacy modes, blocked site data).
    return null
  }
}

export function storeMode(mode: ThemeMode): void {
  try {
    localStorage.setItem(STORAGE_KEY, mode)
  } catch {
    // Still works for this page load; it just won't persist.
  }
}

export function buildTheme(mode: ThemeMode) {
  return createTheme({
    palette: { mode },
    // System font stack instead of MUI's default Roboto, which would need
    // a webfont dependency to actually load.
    typography: {
      fontFamily: 'system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif',
    },
  })
}
