import Paper from '@mui/material/Paper'
import Stack from '@mui/material/Stack'
import Typography from '@mui/material/Typography'
import { useState } from 'react'
import { deleteClient, renameClient, type Client } from '../api'
import ClientCard from './ClientCard'
import ConfirmationModal from './ConfirmationModal'

interface Props {
  clients: Client[]
  /** Refetches the list; awaited after a change so the UI never shows stale data in between. */
  refresh: () => Promise<void>
}

export default function ClientList({ clients, refresh }: Props) {
  // Kept separate from `open` so the dialog's text survives its close
  // animation, and lives here rather than in the card so it isn't torn
  // down when the deleted card disappears from the list.
  const [toDelete, setToDelete] = useState<Client | null>(null)
  const [deleteOpen, setDeleteOpen] = useState(false)

  const rename = async (client: Client, friendlyName: string) => {
    await renameClient(client.client_id, friendlyName)
    await refresh()
  }

  const confirmDelete = async () => {
    if (!toDelete) return
    await deleteClient(toDelete.client_id)
    await refresh()
  }

  return (
    <>
      {clients.length === 0 ? (
        <Paper variant="outlined" sx={{ py: 6, textAlign: 'center' }}>
          <Typography variant="body2" color="text.secondary">
            No clients enrolled yet. Use “Add client” to pair one.
          </Typography>
        </Paper>
      ) : (
        <Stack spacing={1.5} aria-label="Enrolled clients">
          {clients.map((client) => (
            <ClientCard
              key={client.client_id}
              client={client}
              onRename={(name) => rename(client, name)}
              onDelete={() => {
                setToDelete(client)
                setDeleteOpen(true)
              }}
            />
          ))}
        </Stack>
      )}

      <ConfirmationModal
        open={deleteOpen}
        title={`Delete ${toDelete?.friendly_name ?? 'client'}?`}
        message={
          toDelete?.connected
            ? "It's connected right now and will be disconnected. The machine forgets its identity and needs a new pairing code to come back."
            : 'If the machine connects again it will be rejected and forget its identity, and will need a new pairing code to come back.'
        }
        confirmLabel="Delete"
        confirmColor="error"
        onConfirm={confirmDelete}
        onClose={() => setDeleteOpen(false)}
      />
    </>
  )
}
