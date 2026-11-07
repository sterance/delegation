import EditNoteIcon from '@mui/icons-material/EditNote'
import MenuOpenIcon from '@mui/icons-material/MenuOpen'
import Box from '@mui/material/Box'
import Button from '@mui/material/Button'
import IconButton from '@mui/material/IconButton'
import List from '@mui/material/List'
import ListItemButton from '@mui/material/ListItemButton'
import ListItemText from '@mui/material/ListItemText'
import Stack from '@mui/material/Stack'
import Tooltip from '@mui/material/Tooltip'
import Typography from '@mui/material/Typography'
import type { Conversation } from '../../types/chat'

export const SIDEBAR_WIDTH = 260

interface Props {
  conversations: Conversation[]
  activeId: string | null
  onSelect: (id: string) => void
  onNew: () => void
  onCollapse: () => void
}

/** Just the contents; the Chat page decides whether it's docked or a drawer. */
export default function ChatSidebar({ conversations, activeId, onSelect, onNew, onCollapse }: Props) {
  return (
    <Stack sx={{ width: SIDEBAR_WIDTH, height: '100%' }}>
      <Stack direction="row" spacing={1} sx={{ alignItems: 'center', p: 1 }}>
        <Button startIcon={<EditNoteIcon />} onClick={onNew} sx={{ flex: 1, justifyContent: 'flex-start' }}>
          New chat
        </Button>
        <Tooltip title="Hide chats">
          <IconButton onClick={onCollapse} aria-label="Hide chat list">
            <MenuOpenIcon />
          </IconButton>
        </Tooltip>
      </Stack>

      <Box sx={{ flex: 1, minHeight: 0, overflowY: 'auto' }}>
        {conversations.length === 0 ? (
          <Typography variant="body2" color="text.secondary" sx={{ px: 2, py: 1 }}>
            No chats yet
          </Typography>
        ) : (
          <List dense disablePadding sx={{ px: 1 }}>
            {conversations.map((c) => (
              <ListItemButton
                key={c.id}
                selected={c.id === activeId}
                onClick={() => onSelect(c.id)}
                sx={{ borderRadius: 1 }}
              >
                <ListItemText primary={c.title} slotProps={{ primary: { noWrap: true } }} />
              </ListItemButton>
            ))}
          </List>
        )}
      </Box>
    </Stack>
  )
}
