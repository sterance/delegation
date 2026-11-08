import CheckIcon from '@mui/icons-material/Check'
import ListItemIcon from '@mui/material/ListItemIcon'
import ListItemText from '@mui/material/ListItemText'
import ListSubheader from '@mui/material/ListSubheader'
import Menu from '@mui/material/Menu'
import MenuItem from '@mui/material/MenuItem'
import type { ModelOption } from '../../types/chat'

interface Props {
  anchor: HTMLElement | null
  onClose: () => void
  models: ModelOption[]
  selectedId: string | null
  onPick: (model: ModelOption) => void
  /** e.g. "Retry with". */
  heading?: string
  /** Opens upward, for pickers at the bottom of the page. */
  upward?: boolean
}

/** The models the user can chat with, to pick one for an action. */
export default function ModelMenu({ anchor, onClose, models, selectedId, onPick, heading, upward = false }: Props) {
  return (
    <Menu
      anchorEl={anchor}
      open={anchor !== null}
      onClose={onClose}
      anchorOrigin={{ vertical: upward ? 'top' : 'bottom', horizontal: 'left' }}
      transformOrigin={{ vertical: upward ? 'bottom' : 'top', horizontal: 'left' }}
    >
      {heading && <ListSubheader sx={{ lineHeight: 2.5 }}>{heading}</ListSubheader>}
      {models.length === 0 && <MenuItem disabled>No models available, connect a client</MenuItem>}
      {models.map((m) => (
        <MenuItem
          key={m.id}
          selected={m.id === selectedId}
          onClick={() => {
            onPick(m)
            onClose()
          }}
        >
          <ListItemIcon>{m.id === selectedId && <CheckIcon fontSize="small" />}</ListItemIcon>
          <ListItemText primary={m.label} secondary={m.detail} />
        </MenuItem>
      ))}
    </Menu>
  )
}
