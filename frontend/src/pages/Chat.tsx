import EditNoteIcon from '@mui/icons-material/EditNote'
import MenuIcon from '@mui/icons-material/Menu'
import Box from '@mui/material/Box'
import Divider from '@mui/material/Divider'
import Drawer from '@mui/material/Drawer'
import IconButton from '@mui/material/IconButton'
import Stack from '@mui/material/Stack'
import { useTheme } from '@mui/material/styles'
import Tooltip from '@mui/material/Tooltip'
import Typography from '@mui/material/Typography'
import useMediaQuery from '@mui/material/useMediaQuery'
import { useState } from 'react'
import { useChat } from '../hooks/useChat'
import ChatSidebar, { SIDEBAR_WIDTH } from '../components/chat/ChatSidebar'
import Composer from '../components/chat/Composer'
import MessageList, { CHAT_COLUMN_MAX_WIDTH } from '../components/chat/MessageList'

export default function Chat() {
  const chat = useChat()
  const theme = useTheme()
  // Wide screens dock the sidebar beside the chat; narrow ones overlay it.
  const docked = useMediaQuery(theme.breakpoints.up('md'), { noSsr: true })
  const [dockedOpen, setDockedOpen] = useState(true)
  const [overlayOpen, setOverlayOpen] = useState(false)
  const sidebarOpen = docked ? dockedOpen : overlayOpen
  const setSidebarOpen = docked ? setDockedOpen : setOverlayOpen

  // On narrow screens, picking something also gets the overlay out of the way.
  const closeOverlay = () => {
    if (!docked) setOverlayOpen(false)
  }

  const sidebar = (
    <ChatSidebar
      conversations={chat.conversations}
      activeId={chat.active?.id ?? null}
      onSelect={(id) => {
        chat.selectChat(id)
        closeOverlay()
      }}
      onNew={() => {
        chat.newChat()
        closeOverlay()
      }}
      onCollapse={() => setSidebarOpen(false)}
    />
  )

  return (
    <Box sx={{ display: 'flex', height: '100%' }}>
      {docked ? (
        // Width animates to 0 rather than unmounting, so the list keeps its
        // scroll position across collapse/expand.
        <Box
          component="nav"
          aria-label="Chats"
          sx={{
            width: sidebarOpen ? SIDEBAR_WIDTH : 0,
            flexShrink: 0,
            overflow: 'hidden',
            borderRight: sidebarOpen ? 1 : 0,
            borderColor: 'divider',
            bgcolor: 'background.paper',
            transition: theme.transitions.create('width'),
          }}
        >
          {sidebar}
        </Box>
      ) : (
        <Drawer open={overlayOpen} onClose={() => setOverlayOpen(false)}>
          {sidebar}
        </Drawer>
      )}

      <Stack sx={{ flex: 1, minWidth: 0 }}>
        <Stack direction="row" spacing={0.5} sx={{ alignItems: 'center', px: 1, minHeight: 48 }}>
          {!sidebarOpen && (
            <>
              <Tooltip title="Show chats">
                <IconButton onClick={() => setSidebarOpen(true)} aria-label="Show chat list">
                  <MenuIcon />
                </IconButton>
              </Tooltip>
              <Tooltip title="New chat">
                <IconButton onClick={chat.newChat} aria-label="New chat">
                  <EditNoteIcon />
                </IconButton>
              </Tooltip>
            </>
          )}
          <Typography noWrap sx={{ px: 1, fontWeight: 500 }}>
            {chat.active?.title ?? 'New chat'}
          </Typography>
        </Stack>
        <Divider />

        <MessageList key={chat.active?.id ?? 'new'} messages={chat.active?.messages ?? []} />

        <Box sx={{ px: 2, pb: 2, pt: 1 }}>
          <Box sx={{ maxWidth: CHAT_COLUMN_MAX_WIDTH, mx: 'auto' }}>
            <Composer
              onSend={chat.send}
              disabled={chat.busy}
              models={chat.models}
              modelId={chat.modelId}
              onSelectModel={chat.selectModel}
            />
          </Box>
        </Box>
      </Stack>
    </Box>
  )
}
