import CheckCircleIcon from '@mui/icons-material/CheckCircle'
import ContentCopyIcon from '@mui/icons-material/ContentCopy'
import Alert from '@mui/material/Alert'
import Box from '@mui/material/Box'
import Button from '@mui/material/Button'
import CircularProgress from '@mui/material/CircularProgress'
import Dialog from '@mui/material/Dialog'
import DialogActions from '@mui/material/DialogActions'
import DialogContent from '@mui/material/DialogContent'
import DialogTitle from '@mui/material/DialogTitle'
import IconButton from '@mui/material/IconButton'
import Stack from '@mui/material/Stack'
import Tooltip from '@mui/material/Tooltip'
import Typography from '@mui/material/Typography'
import { useEffect, useState } from 'react'
import { ApiError, createPairingCode, errorMessage, getPairingCodeStatus, type ClaimedBy } from '../api'

const STATUS_POLL_MS = 1500
const MONOSPACE = 'ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace'

// loading -> code -> claimed. That's the whole flow; there is deliberately
// no "awaiting verification" step. `error` covers a failed mint or a code
// the server can no longer honour.
type Flow =
  | { step: 'loading' }
  | { step: 'code'; code: string }
  | { step: 'claimed'; client: ClaimedBy }
  | { step: 'error'; message: string }

interface Props {
  open: boolean
  /** `clientAdded` is true if a client claimed the code before the dialog closed. */
  onClose: (clientAdded: boolean) => void
}

export default function AddClientModal({ open, onClose }: Props) {
  const [flow, setFlow] = useState<Flow>({ step: 'loading' })
  // Bumped by "Try again" to mint a fresh code without closing the dialog.
  const [attempt, setAttempt] = useState(0)

  // Every open starts from a clean slate. Reset during render rather than
  // in an effect, so reopening never flashes the previous code or result.
  const [wasOpen, setWasOpen] = useState(open)
  if (open !== wasOpen) {
    setWasOpen(open)
    if (open) setFlow({ step: 'loading' })
  }

  // On open: mint a code.
  useEffect(() => {
    if (!open) return
    let cancelled = false
    createPairingCode().then(
      (code) => {
        if (!cancelled) setFlow({ step: 'code', code })
      },
      (err: unknown) => {
        if (!cancelled) setFlow({ step: 'error', message: `Couldn't create a pairing code: ${errorMessage(err)}` })
      },
    )
    return () => {
      cancelled = true
    }
  }, [open, attempt])

  // Poll while open and unclaimed. Closing the dialog drops `code` to null,
  // which stops polling immediately. The code itself stays valid server-side
  // until it's used or expires — there's nothing to invalidate.
  const code = open && flow.step === 'code' ? flow.code : null
  useEffect(() => {
    if (!code) return
    let cancelled = false
    const timer = setInterval(async () => {
      try {
        const status = await getPairingCodeStatus(code)
        if (cancelled) return
        if (status.used && status.claimed_by) {
          setFlow({ step: 'claimed', client: status.claimed_by })
        } else if (status.used) {
          // The server consumes a code on any enroll attempt, including one
          // rejected because the code had already expired.
          setFlow({
            step: 'error',
            message: 'This code was used after it expired, so no client was enrolled. Generate a new code and try again.',
          })
        }
      } catch (err) {
        if (cancelled) return
        if (err instanceof ApiError && err.status === 404) {
          setFlow({ step: 'error', message: 'The server no longer recognises this pairing code. Generate a new one.' })
        }
        // Anything else (network blip, server restarting) is transient: keep polling.
      }
    }, STATUS_POLL_MS)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [code])

  const clientAdded = flow.step === 'claimed'
  const close = () => onClose(clientAdded)
  const retry = () => {
    setFlow({ step: 'loading' })
    setAttempt((n) => n + 1)
  }

  return (
    <Dialog open={open} onClose={close} maxWidth="xs" fullWidth aria-labelledby="add-client-title">
      <DialogTitle id="add-client-title">Add client</DialogTitle>
      <DialogContent>
        {flow.step === 'loading' && (
          <Box sx={{ display: 'flex', justifyContent: 'center', py: 4 }}>
            <CircularProgress aria-label="Generating pairing code" />
          </Box>
        )}

        {flow.step === 'code' && (
          <>
            <Typography variant="body2" color="text.secondary">
              Enroll the new machine's client with this pairing code. It's single-use and valid for 10 minutes.
            </Typography>
            <Stack direction="row" spacing={1} sx={{ alignItems: 'center', justifyContent: 'center', my: 3 }}>
              <Typography
                component="div"
                sx={{
                  fontFamily: MONOSPACE,
                  fontSize: { xs: '1.75rem', sm: '2.5rem' },
                  fontWeight: 600,
                  letterSpacing: '0.15em',
                }}
              >
                {flow.code}
              </Typography>
              <CopyButton text={flow.code} />
            </Stack>
            <Stack direction="row" spacing={1.5} sx={{ alignItems: 'center' }}>
              <CircularProgress size={16} />
              <Typography variant="body2" color="text.secondary">
                Waiting for a client to use this code…
              </Typography>
            </Stack>
          </>
        )}

        {flow.step === 'claimed' && (
          <Stack spacing={2} sx={{ alignItems: 'center', py: 1 }}>
            <CheckCircleIcon color="success" sx={{ fontSize: 56 }} />
            <Typography variant="h6">Client enrolled</Typography>
            <Box sx={{ width: '100%' }}>
              <Detail label="Hostname" value={flow.client.hostname} />
              <Detail label="Client ID" value={flow.client.client_id} monospace />
            </Box>
          </Stack>
        )}

        {flow.step === 'error' && <Alert severity="error">{flow.message}</Alert>}
      </DialogContent>
      <DialogActions>
        {flow.step === 'claimed' ? (
          <Button variant="contained" onClick={close} autoFocus>
            Done
          </Button>
        ) : flow.step === 'error' ? (
          <>
            <Button onClick={close}>Close</Button>
            <Button variant="contained" onClick={retry}>
              Try again
            </Button>
          </>
        ) : (
          <Button onClick={close}>Cancel</Button>
        )}
      </DialogActions>
    </Dialog>
  )
}

function Detail({ label, value, monospace = false }: { label: string; value: string; monospace?: boolean }) {
  return (
    <Box sx={{ mb: 1.5 }}>
      <Typography variant="caption" color="text.secondary" component="div">
        {label}
      </Typography>
      <Typography
        variant="body2"
        sx={{ fontFamily: monospace ? MONOSPACE : undefined, wordBreak: 'break-all' }}
      >
        {value}
      </Typography>
    </Box>
  )
}

function CopyButton({ text }: { text: string }) {
  const [result, setResult] = useState<'idle' | 'copied' | 'failed'>('idle')

  useEffect(() => {
    if (result === 'idle') return
    const timer = setTimeout(() => setResult('idle'), 2000)
    return () => clearTimeout(timer)
  }, [result])

  const copy = async () => {
    try {
      // navigator.clipboard only exists in secure contexts (https or
      // localhost); anywhere else this throws and we say so.
      await navigator.clipboard.writeText(text)
      setResult('copied')
    } catch {
      setResult('failed')
    }
  }

  const title = result === 'copied' ? 'Copied!' : result === 'failed' ? "Couldn't copy — select it manually" : 'Copy code'
  return (
    <Tooltip title={title}>
      <IconButton onClick={copy} aria-label="Copy pairing code">
        <ContentCopyIcon />
      </IconButton>
    </Tooltip>
  )
}
