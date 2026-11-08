import DeleteOutlinedIcon from '@mui/icons-material/DeleteOutlined'
import EditNoteIcon from '@mui/icons-material/EditNote'
import EditOutlinedIcon from '@mui/icons-material/EditOutlined'
import MenuOpenIcon from '@mui/icons-material/MenuOpen'
import MoreVertIcon from '@mui/icons-material/MoreVert'
import Alert from '@mui/material/Alert'
import Box from '@mui/material/Box'
import Button from '@mui/material/Button'
import IconButton from '@mui/material/IconButton'
import List from '@mui/material/List'
import ListItem from '@mui/material/ListItem'
import ListItemButton from '@mui/material/ListItemButton'
import ListItemIcon from '@mui/material/ListItemIcon'
import ListItemText from '@mui/material/ListItemText'
import Menu from '@mui/material/Menu'
import MenuItem from '@mui/material/MenuItem'
import Stack from '@mui/material/Stack'
import Tooltip from '@mui/material/Tooltip'
import Typography from '@mui/material/Typography'
import { useState } from 'react'
import type { ConversationSummary } from '../../types/chat'
import ConfirmationModal from '../common/ConfirmationModal'
import TextPromptModal from '../common/TextPromptModal'

export const SIDEBAR_WIDTH = 260

/** Must match RENAME_MAX_CHARS in server/src/conversations.rs. */
const TITLE_MAX_LENGTH = 200

interface Props {
  conversations: ConversationSummary[]
  activeId: string | null
  hasMore: boolean
  listError: string | null
  onSelect: (id: string) => void
  onNew: () => void
  onCollapse: () => void
  onLoadMore: () => void
  onRename: (id: string, title: string) => Promise<void>
  onDelete: (id: string) => Promise<void>
}

/** Just the contents; the Chat page decides whether it's docked or a drawer. */
export default function ChatSidebar({
  conversations,
  activeId,
  hasMore,
  listError,
  onSelect,
  onNew,
  onCollapse,
  onLoadMore,
  onRename,
  onDelete,
}: Props) {
  const [menu, setMenu] = useState<{ anchor: HTMLElement; chat: ConversationSummary } | null>(null)
  // Kept apart from `open` so the dialogs' text survives their close animation.
  const [target, setTarget] = useState<ConversationSummary | null>(null)
  const [dialog, setDialog] = useState<'rename' | 'delete' | null>(null)

  const choose = (kind: 'rename' | 'delete') => {
    setTarget(menu?.chat ?? null)
    setDialog(kind)
    setMenu(null)
  }

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
        {listError && (
          <Alert severity="warning" sx={{ mx: 1, mb: 1 }}>
            Couldn't load your chats: {listError}
          </Alert>
        )}
        {conversations.length === 0 ? (
          <Typography variant="body2" color="text.secondary" sx={{ px: 2, py: 1 }}>
            No chats yet
          </Typography>
        ) : (
          <List dense disablePadding sx={{ px: 1 }}>
            {conversations.map((c) => (
              <ListItem
                key={c.id}
                disablePadding
                secondaryAction={
                  <IconButton
                    edge="end"
                    size="small"
                    aria-label={`Options for ${c.title}`}
                    aria-haspopup="menu"
                    onClick={(e) => setMenu({ anchor: e.currentTarget, chat: c })}
                    className="chat-options"
                    sx={{ opacity: { xs: 1, md: c.id === activeId ? 1 : 0 }, '&:focus-visible': { opacity: 1 } }}
                  >
                    <MoreVertIcon fontSize="small" />
                  </IconButton>
                }
                sx={{ '&:hover .chat-options': { opacity: 1 } }}
              >
                <ListItemButton
                  selected={c.id === activeId}
                  onClick={() => onSelect(c.id)}
                  sx={{ borderRadius: 1, pr: 5 }}
                >
                  <ListItemText primary={c.title} slotProps={{ primary: { noWrap: true } }} />
                </ListItemButton>
              </ListItem>
            ))}
          </List>
        )}
        {hasMore && (
          <Box sx={{ px: 1, py: 1 }}>
            <Button size="small" fullWidth onClick={onLoadMore}>
              Show older chats
            </Button>
          </Box>
        )}
      </Box>

      <Menu anchorEl={menu?.anchor ?? null} open={menu !== null} onClose={() => setMenu(null)}>
        <MenuItem onClick={() => choose('rename')}>
          <ListItemIcon>
            <EditOutlinedIcon fontSize="small" />
          </ListItemIcon>
          Rename
        </MenuItem>
        <MenuItem onClick={() => choose('delete')}>
          <ListItemIcon>
            <DeleteOutlinedIcon fontSize="small" />
          </ListItemIcon>
          Delete
        </MenuItem>
      </Menu>

      <TextPromptModal
        open={dialog === 'rename'}
        title="Rename chat"
        label="Title"
        initialValue={target?.title ?? ''}
        maxLength={TITLE_MAX_LENGTH}
        confirmLabel="Save"
        onConfirm={(title) => (target ? onRename(target.id, title) : Promise.resolve())}
        onClose={() => setDialog(null)}
      />
      <ConfirmationModal
        open={dialog === 'delete'}
        title="Delete this chat?"
        message={`“${target?.title ?? ''}” and every message in it will be deleted. This can't be undone.`}
        confirmLabel="Delete"
        confirmColor="error"
        onConfirm={() => (target ? onDelete(target.id) : undefined)}
        onClose={() => setDialog(null)}
      />
    </Stack>
  )
}
