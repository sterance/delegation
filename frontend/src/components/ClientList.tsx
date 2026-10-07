import Box from '@mui/material/Box'
import Paper from '@mui/material/Paper'
import Stack from '@mui/material/Stack'
import Table from '@mui/material/Table'
import TableBody from '@mui/material/TableBody'
import TableCell from '@mui/material/TableCell'
import TableContainer from '@mui/material/TableContainer'
import TableHead from '@mui/material/TableHead'
import TableRow from '@mui/material/TableRow'
import Typography from '@mui/material/Typography'
import type { Client } from '../api'

const timestampFormat = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'medium' })

function formatTimestamp(iso: string | null): string {
  if (!iso) return 'Never'
  const date = new Date(iso)
  return Number.isNaN(date.getTime()) ? iso : timestampFormat.format(date)
}

/** Same green/red convention as the tray client: green = connected, red = not. */
function StatusDot({ connected }: { connected: boolean }) {
  return (
    <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
      <Box
        component="span"
        aria-hidden
        sx={{
          width: 10,
          height: 10,
          borderRadius: '50%',
          flexShrink: 0,
          bgcolor: connected ? 'success.main' : 'error.main',
        }}
      />
      <Typography variant="body2">{connected ? 'Connected' : 'Disconnected'}</Typography>
    </Stack>
  )
}

export default function ClientList({ clients }: { clients: Client[] }) {
  return (
    <TableContainer component={Paper} variant="outlined">
      <Table aria-label="Enrolled clients">
        <TableHead>
          <TableRow>
            <TableCell>Status</TableCell>
            <TableCell>Hostname</TableCell>
            <TableCell>Enrolled</TableCell>
            <TableCell>Last seen</TableCell>
          </TableRow>
        </TableHead>
        <TableBody>
          {clients.length === 0 ? (
            <TableRow>
              <TableCell colSpan={4} align="center" sx={{ py: 6, color: 'text.secondary' }}>
                No clients enrolled yet. Use “Add client” to pair one.
              </TableCell>
            </TableRow>
          ) : (
            clients.map((client) => (
              <TableRow key={client.client_id}>
                <TableCell>
                  <StatusDot connected={client.connected} />
                </TableCell>
                <TableCell>
                  <Typography variant="body2" sx={{ fontWeight: 500 }}>
                    {client.hostname}
                  </Typography>
                  <Typography variant="caption" color="text.secondary" sx={{ fontFamily: 'monospace' }}>
                    {client.client_id}
                  </Typography>
                </TableCell>
                <TableCell title={client.enrolled_at}>{formatTimestamp(client.enrolled_at)}</TableCell>
                <TableCell title={client.last_seen_at ?? undefined}>{formatTimestamp(client.last_seen_at)}</TableCell>
              </TableRow>
            ))
          )}
        </TableBody>
      </Table>
    </TableContainer>
  )
}
