import KeyboardArrowDownIcon from '@mui/icons-material/KeyboardArrowDown'
import Box from '@mui/material/Box'
import Button from '@mui/material/Button'
import { useState } from 'react'
import type { ModelOption } from '../../types/chat'
import ModelMenu from './ModelMenu'

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
        sx={{ textTransform: 'none', color: 'text.secondary', minWidth: 0 }}
      >
        <Box component="span" sx={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
          {selected ? `${selected.label} · ${selected.detail}` : 'Model'}
        </Box>
      </Button>
      <ModelMenu
        anchor={anchor}
        onClose={() => setAnchor(null)}
        models={models}
        selectedId={selectedId}
        onPick={(m) => onSelect(m.id)}
        upward
      />
    </>
  )
}
