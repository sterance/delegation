import ChevronLeftIcon from '@mui/icons-material/ChevronLeft'
import ChevronRightIcon from '@mui/icons-material/ChevronRight'
import IconButton from '@mui/material/IconButton'
import Stack from '@mui/material/Stack'
import Typography from '@mui/material/Typography'
import type { ChatMessage } from '../../types/chat'

interface Props {
  position: number
  siblings: ChatMessage[]
  onSwitch: (sibling: ChatMessage) => void
  disabled?: boolean
}

/** `‹ 2 / 3 ›` between alternative versions of a message. */
export default function BranchSwitcher({ position, siblings, onSwitch, disabled = false }: Props) {
  if (siblings.length < 2) return null
  const go = (to: number) => onSwitch(siblings[to - 1])
  return (
    <Stack direction="row" sx={{ alignItems: 'center' }} role="group" aria-label="Versions">
      <IconButton
        size="small"
        onClick={() => go(position - 1)}
        disabled={disabled || position <= 1}
        aria-label="Previous version"
      >
        <ChevronLeftIcon fontSize="small" />
      </IconButton>
      <Typography variant="caption" color="text.secondary" sx={{ minWidth: '2.5em', textAlign: 'center' }}>
        {position} / {siblings.length}
      </Typography>
      <IconButton
        size="small"
        onClick={() => go(position + 1)}
        disabled={disabled || position >= siblings.length}
        aria-label="Next version"
      >
        <ChevronRightIcon fontSize="small" />
      </IconButton>
    </Stack>
  )
}
