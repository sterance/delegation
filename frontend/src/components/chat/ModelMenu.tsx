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

/**
 * The models the user can chat with, to pick one for an action: "any of my
 * clients" entries first, then each client's.
 */
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
      {models.length === 0 && (
        <MenuItem disabled>No models available: connect a client, or install one from Clients</MenuItem>
      )}
      {models.flatMap((m, i) => {
        // The list comes sorted: pool entries, then client by client. A new
        // heading starts a new group.
        const group = i === 0 || models[i - 1].detail !== m.detail
        const item = (
          <MenuItem
            key={m.id}
            selected={m.id === selectedId}
            onClick={() => {
              onPick(m)
              onClose()
            }}
          >
            <ListItemIcon>{m.id === selectedId && <CheckIcon fontSize="small" />}</ListItemIcon>
            <ListItemText primary={m.label} secondary={m.note} />
          </MenuItem>
        )
        return group
          ? [
              <ListSubheader key={`group-${m.id}`} sx={{ lineHeight: 2.5 }}>
                {m.detail}
              </ListSubheader>,
              item,
            ]
          : [item]
      })}
    </Menu>
  )
}
