import Box from '@mui/material/Box'
import type { ReactNode } from 'react'
import ThemeToggle from '../common/ThemeToggle'

/**
 * The narrow centred column the login, setup and reset pages share, with
 * the site logo on top (laid out like sterance/travel-planner's login).
 * The logo is a blue tile with a white mark, which reads well in both
 * themes, so it isn't inverted in light mode.
 */
export default function AuthLayout({ children }: { children: ReactNode }) {
  return (
    <Box sx={{ minHeight: '100dvh', position: 'relative' }}>
      <Box sx={{ position: 'absolute', top: 8, right: 8 }}>
        <ThemeToggle />
      </Box>
      <Box
        component="main"
        sx={{ width: '100%', maxWidth: 440, mx: 'auto', px: { xs: 2, sm: 2 }, pt: { xs: 6, sm: 8 }, pb: 4 }}
      >
        <Box
          component="img"
          src="/favicon.svg"
          alt="Delegation"
          sx={{ display: 'block', width: '60%', maxWidth: 220, height: 'auto', mx: 'auto' }}
        />
        {children}
      </Box>
    </Box>
  )
}
