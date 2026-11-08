import AddIcon from '@mui/icons-material/Add'
import Alert from '@mui/material/Alert'
import Box from '@mui/material/Box'
import Button from '@mui/material/Button'
import CircularProgress from '@mui/material/CircularProgress'
import Container from '@mui/material/Container'
import MenuItem from '@mui/material/MenuItem'
import Stack from '@mui/material/Stack'
import TextField from '@mui/material/TextField'
import Typography from '@mui/material/Typography'
import { useEffect, useMemo, useState } from 'react'
import { DEFAULT_DASHBOARD_CONFIG, errorMessage, getDashboardConfig, type Client, type DashboardConfig } from '../api'
import { useAuth } from '../auth/context'
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
        console.error(`Couldn't load /api/dashboard-config, using defaults: ${errorMessage(err)}`)
        if (!cancelled) setConfig(DEFAULT_DASHBOARD_CONFIG)
      },
    )
    return () => {
      cancelled = true
    }
  }, [])
  return config
}

/** The admin's filter: everyone's clients, their own, or one user's (`user:<id>`). */
type OwnerFilter = 'all' | 'mine' | `user:${string}`

/** Everyone but `meId` who owns at least one of `clients`, by name. */
function otherOwners(clients: Client[] | null, meId: string): { id: string; username: string }[] {
  const owners = new Map<string, string>()
  for (const c of clients ?? []) if (c.owner.user_id !== meId) owners.set(c.owner.user_id, c.owner.username)
  return [...owners].map(([id, username]) => ({ id, username })).sort((a, b) => a.username.localeCompare(b.username))
}

export default function Clients() {
  const config = useDashboardConfig()
  const { user } = useAuth()
  const meId = user?.id ?? ''
  const isAdmin = user?.role === 'admin'
  const { clients, history, error, refresh } = useClients(config)
  const [addOpen, setAddOpen] = useState(false)
  const [filter, setFilter] = useState<OwnerFilter>('all')

  const handleAddClose = (clientAdded: boolean) => {
    setAddOpen(false)
    if (clientAdded) void refresh()
  }

  const owners = useMemo(() => otherOwners(clients, meId), [clients, meId])
  const shown = useMemo(() => {
    if (!clients || !isAdmin || filter === 'all') return clients
    const ownerId = filter === 'mine' ? meId : filter.slice('user:'.length)
    return clients.filter((c) => c.owner.user_id === ownerId)
  }, [clients, isAdmin, filter, meId])
  const connectedCount = shown?.filter((c) => c.connected).length ?? 0

  return (
    <>
      <Container maxWidth="lg" sx={{ py: 4 }}>
        <Stack direction="row" spacing={2} sx={{ alignItems: 'center', justifyContent: 'space-between', mb: 2 }}>
          <Box>
            <Typography variant="h5" component="h2">
              Clients
            </Typography>
            {shown && (
              <Typography variant="body2" color="text.secondary">
                {connectedCount} of {shown.length} connected
              </Typography>
            )}
          </Box>
          <Stack direction="row" spacing={1.5} sx={{ alignItems: 'center' }}>
            {isAdmin && (
              <TextField
                select
                size="small"
                label="Owner"
                value={filter === 'all' || filter === 'mine' || owners.some((o) => filter === `user:${o.id}`) ? filter : 'all'}
                onChange={(e) => setFilter(e.target.value as OwnerFilter)}
                sx={{ minWidth: 130 }}
              >
                <MenuItem value="all">All</MenuItem>
                <MenuItem value="mine">Mine</MenuItem>
                {owners.map((o) => (
                  <MenuItem key={o.id} value={`user:${o.id}`}>
                    {o.username}
                  </MenuItem>
                ))}
              </TextField>
            )}
            <Button variant="contained" startIcon={<AddIcon />} onClick={() => setAddOpen(true)}>
              Add client
            </Button>
          </Stack>
        </Stack>

        {error && (
          <Alert severity={clients ? 'warning' : 'error'} sx={{ mb: 2 }}>
            {clients
              ? `Couldn't refresh the client list (${error}). Statuses below may be out of date.`
              : `Couldn't load the client list (${error}). Retrying…`}
          </Alert>
        )}

        {shown && config ? (
          <ClientList clients={shown} history={history} config={config} refresh={refresh} meId={meId} />
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
