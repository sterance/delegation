import AddIcon from '@mui/icons-material/Add'
import Alert from '@mui/material/Alert'
import Box from '@mui/material/Box'
import Button from '@mui/material/Button'
import CircularProgress from '@mui/material/CircularProgress'
import Container from '@mui/material/Container'
import Stack from '@mui/material/Stack'
import Typography from '@mui/material/Typography'
import { useEffect, useState } from 'react'
import { DEFAULT_DASHBOARD_CONFIG, errorMessage, getDashboardConfig, type DashboardConfig } from '../api'
import AddClientModal from '../components/clients/AddClientModal'
import ClientList from '../components/clients/ClientList'
import { useClients } from '../hooks/useClients'

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
        console.error(`Couldn't load /api/admin/dashboard-config, using defaults: ${errorMessage(err)}`)
        if (!cancelled) setConfig(DEFAULT_DASHBOARD_CONFIG)
      },
    )
    return () => {
      cancelled = true
    }
  }, [])
  return config
}

export default function Clients() {
  const config = useDashboardConfig()
  const { clients, history, error, refresh } = useClients(config)
  const [addOpen, setAddOpen] = useState(false)

  const handleAddClose = (clientAdded: boolean) => {
    setAddOpen(false)
    if (clientAdded) void refresh()
  }

  const connectedCount = clients?.filter((c) => c.connected).length ?? 0

  return (
    <>
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
    </>
  )
}
