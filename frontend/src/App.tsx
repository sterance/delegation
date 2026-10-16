import AddIcon from '@mui/icons-material/Add'
import Brightness4Icon from '@mui/icons-material/Brightness4'
import Brightness7Icon from '@mui/icons-material/Brightness7'
import Alert from '@mui/material/Alert'
import AppBar from '@mui/material/AppBar'
import Box from '@mui/material/Box'
import Button from '@mui/material/Button'
import CircularProgress from '@mui/material/CircularProgress'
import Container from '@mui/material/Container'
import CssBaseline from '@mui/material/CssBaseline'
import IconButton from '@mui/material/IconButton'
import Stack from '@mui/material/Stack'
import { ThemeProvider } from '@mui/material/styles'
import Toolbar from '@mui/material/Toolbar'
import Tooltip from '@mui/material/Tooltip'
import Typography from '@mui/material/Typography'
import useMediaQuery from '@mui/material/useMediaQuery'
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { errorMessage, listClients, type Client } from './api'
import AddClientModal from './components/AddClientModal'
import ClientList from './components/ClientList'
import { buildTheme, readStoredMode, storeMode, type ThemeMode } from './theme'

const CLIENTS_POLL_MS = 4000

/**
 * Follows the browser/OS preference live until the user explicitly toggles,
 * after which their choice is persisted and wins.
 */
function useThemeMode(): [ThemeMode, () => void] {
  // noSsr: there's no server render, so read the real value on the first
  // render instead of flashing light first. useMediaQuery subscribes to the
  // query's `change` event, so an OS theme switch is picked up live.
  const prefersDark = useMediaQuery('(prefers-color-scheme: dark)', { noSsr: true })
  const [override, setOverride] = useState<ThemeMode | null>(readStoredMode)
  const mode = override ?? (prefersDark ? 'dark' : 'light')

  const toggle = useCallback(() => {
    const next = mode === 'dark' ? 'light' : 'dark'
    storeMode(next)
    setOverride(next)
  }, [mode])

  return [mode, toggle]
}

/** Polls GET /clients; `refresh` fetches immediately without waiting for the next tick. */
function useClients() {
  const [clients, setClients] = useState<Client[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  // Requests can overlap (slow server, or a manual refresh mid-poll), so
  // never let an older response overwrite a newer one.
  const issued = useRef(0)
  const applied = useRef(0)

  const refresh = useCallback(async () => {
    const id = ++issued.current
    try {
      const result = await listClients()
      if (id < applied.current) return
      applied.current = id
      setClients(result)
      setError(null)
    } catch (err) {
      if (id < applied.current) return
      applied.current = id
      setError(errorMessage(err))
    }
  }, [])

  useEffect(() => {
    // refresh() only sets state after its fetch resolves, never synchronously.
    // oxlint-disable-next-line react/set-state-in-effect
    void refresh()
    const timer = setInterval(() => void refresh(), CLIENTS_POLL_MS)
    return () => clearInterval(timer)
  }, [refresh])

  return { clients, error, refresh }
}

export default function App() {
  const [mode, toggleMode] = useThemeMode()
  const theme = useMemo(() => buildTheme(mode), [mode])
  const { clients, error, refresh } = useClients()
  const [addOpen, setAddOpen] = useState(false)

  const handleAddClose = (clientAdded: boolean) => {
    setAddOpen(false)
    if (clientAdded) void refresh()
  }

  const connectedCount = clients?.filter((c) => c.connected).length ?? 0

  return (
    <ThemeProvider theme={theme}>
      <CssBaseline enableColorScheme />
      <AppBar position="static">
        <Toolbar>
          <Typography variant="h6" component="h1" sx={{ flexGrow: 1 }}>
            Delegation
          </Typography>
          <Tooltip title={mode === 'dark' ? 'Switch to light mode' : 'Switch to dark mode'}>
            <IconButton color="inherit" onClick={toggleMode} aria-label="Toggle light/dark mode">
              {mode === 'dark' ? <Brightness7Icon /> : <Brightness4Icon />}
            </IconButton>
          </Tooltip>
        </Toolbar>
      </AppBar>

      <Container maxWidth="lg" sx={{ py: 4 }}>
        <Stack direction="row" spacing={2} sx={{ alignItems: 'center', justifyContent: 'space-between', mb: 2 }}>
          <Box>
            <Typography variant="h5" component="h2">
              Clients
            </Typography>
            {clients && (
              <Typography variant="body2" color="text.secondary">
                {connectedCount} of {clients.length} connected
              </Typography>
            )}
          </Box>
          <Button variant="contained" startIcon={<AddIcon />} onClick={() => setAddOpen(true)}>
            Add client
          </Button>
        </Stack>

        {error && (
          <Alert severity={clients ? 'warning' : 'error'} sx={{ mb: 2 }}>
            {clients
              ? `Couldn't refresh the client list (${error}). Statuses below may be out of date.`
              : `Couldn't load the client list (${error}). Retrying…`}
          </Alert>
        )}

        {clients ? (
          <ClientList clients={clients} refresh={refresh} />
        ) : (
          !error && (
            <Box sx={{ display: 'flex', justifyContent: 'center', py: 6 }}>
              <CircularProgress aria-label="Loading clients" />
            </Box>
          )
        )}
      </Container>

      <AddClientModal open={addOpen} onClose={handleAddClose} />
    </ThemeProvider>
  )
}
