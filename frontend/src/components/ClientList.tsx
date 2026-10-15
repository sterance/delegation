import Paper from '@mui/material/Paper'
import Stack from '@mui/material/Stack'
import Typography from '@mui/material/Typography'
import type { Client } from '../api'
import ClientCard from './ClientCard'

export default function ClientList({ clients }: { clients: Client[] }) {
  if (clients.length === 0) {
    return (
      <Paper variant="outlined" sx={{ py: 6, textAlign: 'center' }}>
        <Typography variant="body2" color="text.secondary">
          No clients enrolled yet. Use “Add client” to pair one.
        </Typography>
      </Paper>
    )
  }

  return (
    <Stack spacing={1.5} aria-label="Enrolled clients">
      {clients.map((client) => (
        <ClientCard key={client.client_id} client={client} />
      ))}
    </Stack>
  )
}
