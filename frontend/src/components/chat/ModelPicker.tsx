import CheckIcon from '@mui/icons-material/Check'
import KeyboardArrowDownIcon from '@mui/icons-material/KeyboardArrowDown'
import Button from '@mui/material/Button'
import ListItemIcon from '@mui/material/ListItemIcon'
import ListItemText from '@mui/material/ListItemText'
import Menu from '@mui/material/Menu'
import MenuItem from '@mui/material/MenuItem'
import { useState } from 'react'
import type { ModelOption } from '../../types/chat'

interface Props {
  models: ModelOption[]
  selectedId: string | null
  onSelect: (id: string) => void
  /** Called each time the menu opens, to refresh the list. */
  onOpen?: () => void
}

export default function ModelPicker({ models, selectedId, onSelect, onOpen }: Props) {
  const [anchor, setAnchor] = useState<HTMLElement | null>(null)
  const selected = models.find((m) => m.id === selectedId)

  return (
    <>
      <Button
        size="small"
        color="inherit"
        endIcon={<KeyboardArrowDownIcon />}
        onClick={(e) => {
          setAnchor(e.currentTarget)
          onOpen?.()
        }}
        aria-haspopup="menu"
        aria-expanded={anchor ? 'true' : undefined}
        sx={{ textTransform: 'none', color: 'text.secondary' }}
      >
        {selected ? `${selected.label}${selected.detail ? ` · ${selected.detail}` : ''}` : 'Model'}
      </Button>
      <Menu
        anchorEl={anchor}
        open={anchor !== null}
        onClose={() => setAnchor(null)}
        // Opens upward from the composer, which sits at the bottom of the page.
        anchorOrigin={{ vertical: 'top', horizontal: 'left' }}
        transformOrigin={{ vertical: 'bottom', horizontal: 'left' }}
      >
        {models.length === 0 && <MenuItem disabled>No models available, connect a client</MenuItem>}
        {models.map((m) => (
          <MenuItem
            key={m.id}
            selected={m.id === selectedId}
            onClick={() => {
              onSelect(m.id)
              setAnchor(null)
            }}
          >
            <ListItemIcon>{m.id === selectedId && <CheckIcon fontSize="small" />}</ListItemIcon>
            <ListItemText primary={m.label} secondary={m.detail} />
          </MenuItem>
        ))}
      </Menu>
    </>
  )
}
