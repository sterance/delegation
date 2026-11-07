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
import { useCallback, useEffect, useMemo, useState } from 'react'
import { DEFAULT_DASHBOARD_CONFIG, errorMessage, getDashboardConfig, type DashboardConfig } from './api'
import AddClientModal from './components/AddClientModal'
import ClientList from './components/ClientList'
import { buildTheme, readStoredMode, storeMode, type ThemeMode } from './theme'
import { useClients } from './useClients'

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

/**
 * The server's DASHBOARD_* settings, fetched once. Null until loaded; if
 * that fails, the built-in defaults (the same values the server defaults to).
 */
function useDashboardConfig(): DashboardConfig | null {
  const [config, setConfig] = useState<DashboardConfig | null>(null)
  useEffect(() => {
    let cancelled = false
    getDashboardConfig().then(
      (loaded) => {
        if (!cancelled) setConfig(loaded)
      },
      (err: unknown) => {
        console.error(`Couldn't load /admin/dashboard-config, using defaults: ${errorMessage(err)}`)
        if (!cancelled) setConfig(DEFAULT_DASHBOARD_CONFIG)
      },
    )
    return () => {
      cancelled = true
    }
  }, [])
  return config
}

export default function App() {
  const [mode, toggleMode] = useThemeMode()
  const theme = useMemo(() => buildTheme(mode), [mode])
  const config = useDashboardConfig()
  const { clients, history, error, refresh } = useClients(config)
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

        {clients && config ? (
          <ClientList clients={clients} history={history} config={config} refresh={refresh} />
        ) : (
          !error && (
            <Box sx={{ display: 'flex', justifyContent: 'center', py: 6 }}>
              <CircularProgress aria-label="Loading clients" />
            </Box>
          )
        )}
      </Container>

      <AddClientModal
        open={addOpen}
        onClose={handleAddClose}
        codeTtlSecs={(config ?? DEFAULT_DASHBOARD_CONFIG).pairing_code_ttl_secs}
        statusPollMs={(config ?? DEFAULT_DASHBOARD_CONFIG).pairing_poll_interval_ms}
      />
    </ThemeProvider>
  )
}
