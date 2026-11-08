import Visibility from '@mui/icons-material/Visibility'
import VisibilityOff from '@mui/icons-material/VisibilityOff'
import IconButton from '@mui/material/IconButton'
import InputAdornment from '@mui/material/InputAdornment'
import TextField from '@mui/material/TextField'
import { useState, type ChangeEvent } from 'react'

interface Props {
  label: string
  value: string
  onChange: (event: ChangeEvent<HTMLInputElement>) => void
  /** `current-password` or `new-password`, so password managers fill it. */
  autoComplete: 'current-password' | 'new-password'
  disabled?: boolean
  name?: string
  autoFocus?: boolean
}

/** A password field with a show/hide button. */
export default function PasswordField({ label, value, onChange, autoComplete, disabled = false, name, autoFocus }: Props) {
  const [visible, setVisible] = useState(false)
  return (
    <TextField
      fullWidth
      label={label}
      type={visible ? 'text' : 'password'}
      name={name}
      autoComplete={autoComplete}
      value={value}
      onChange={onChange}
      margin="normal"
      disabled={disabled}
      // oxlint-disable-next-line jsx-a11y/no-autofocus
      autoFocus={autoFocus}
      slotProps={{
        input: {
          endAdornment: (
            <InputAdornment position="end">
              <IconButton
                aria-label={visible ? 'Hide password' : 'Show password'}
                onClick={() => setVisible((v) => !v)}
                edge="end"
                disabled={disabled}
              >
                {visible ? <VisibilityOff /> : <Visibility />}
              </IconButton>
            </InputAdornment>
          ),
        },
      }}
    />
  )
}
