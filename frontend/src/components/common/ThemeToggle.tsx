import Brightness4Icon from '@mui/icons-material/Brightness4'
import Brightness7Icon from '@mui/icons-material/Brightness7'
import IconButton from '@mui/material/IconButton'
import Tooltip from '@mui/material/Tooltip'
import { useThemeModeToggle } from '../../themeMode'

export default function ThemeToggle({ color = 'default' }: { color?: 'default' | 'inherit' }) {
  const { mode, toggle } = useThemeModeToggle()
  return (
    <Tooltip title={mode === 'dark' ? 'Switch to light mode' : 'Switch to dark mode'}>
      <IconButton color={color} onClick={toggle} aria-label="Toggle light/dark mode">
        {mode === 'dark' ? <Brightness7Icon /> : <Brightness4Icon />}
      </IconButton>
    </Tooltip>
  )
}
