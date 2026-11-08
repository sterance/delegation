import DeleteOutlinedIcon from '@mui/icons-material/DeleteOutlined'
import DownloadIcon from '@mui/icons-material/Download'
import Alert from '@mui/material/Alert'
import Box from '@mui/material/Box'
import Button from '@mui/material/Button'
import IconButton from '@mui/material/IconButton'
import LinearProgress from '@mui/material/LinearProgress'
import ListItemText from '@mui/material/ListItemText'
import Menu from '@mui/material/Menu'
import MenuItem from '@mui/material/MenuItem'
import Stack from '@mui/material/Stack'
import Tooltip from '@mui/material/Tooltip'
import Typography from '@mui/material/Typography'
import { useCallback, useEffect, useState } from 'react'
import {
  cancelModelCommand,
  deleteModel,
  errorMessage,
  getClientModels,
  installModel,
  isCommandActive,
  listCatalog,
  type CatalogEntry,
  type ClientModels as Detail,
  type DashboardConfig,
  type LocalModel,
  type ModelCommand,
  type Progress,
} from '../../api'
import type { ModelState } from '../../types/chat'
import { formatBytes } from '../../utils/format'
import ConfirmationModal from '../common/ConfirmationModal'

const STATE_LABEL: Record<ModelState, string> = {
  loaded: 'Loaded',
  loading: 'Loading…',
  available: 'Available',
  downloading: 'Downloading',
  failed: 'Failed',
}

const RUNTIME_LABEL = {
  ready: 'ready',
  downloading: 'downloading…',
  absent: 'downloaded with the first model',
} as const

/** A row: a model on the client, or one a pull is starting to fetch. */
interface Row {
  id: string
  name: string
  state: ModelState
  size: number | null
  error: string | null
  builtIn: boolean
  /** The running pull for it, if any. */
  pull: ModelCommand | null
}

function rows(detail: Detail): Row[] {
  const pulls = detail.commands.filter((c) => c.kind === 'pull' && isCommandActive(c.state))
  const pullFor = (id: string) => pulls.find((c) => c.model_id === id) ?? null
  const list: Row[] = detail.models.map((m: LocalModel) => ({
    id: m.id,
    name: m.display_name,
    state: m.state,
    size: m.backend === 'echo' ? null : m.size_bytes,
    error: m.error,
    builtIn: m.backend === 'echo',
    pull: pullFor(m.id),
  }))
  // Sent, but not in the client's inventory yet.
  for (const p of pulls) {
    if (!list.some((r) => r.id === p.model_id)) {
      list.push({
        id: p.model_id,
        name: p.model_name,
        state: 'downloading',
        size: p.progress?.total_bytes ?? null,
        error: null,
        builtIn: false,
        pull: p,
      })
    }
  }
  return list
}

function progressText(p: Progress | null): string {
  if (!p) return 'Starting…'
  const what = p.phase === 'runtime' ? 'llama.cpp first: ' : ''
  return `${what}${formatBytes(p.done_bytes)} of ${formatBytes(p.total_bytes)}`
}

function percent(p: Progress | null): number | undefined {
  return p && p.total_bytes > 0 ? (100 * p.done_bytes) / p.total_bytes : undefined
}

interface Props {
  clientId: string
  clientName: string
  config: DashboardConfig
}

/** The expanded card's Models section: what's installed, what's downloading, and installing more. */
export default function ClientModels({ clientId, clientName, config }: Props) {
  const [detail, setDetail] = useState<Detail | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [catalog, setCatalog] = useState<CatalogEntry[]>([])
  const [menu, setMenu] = useState<HTMLElement | null>(null)
  const [toDelete, setToDelete] = useState<Row | null>(null)
  const [deleteOpen, setDeleteOpen] = useState(false)
  const [dismissed, setDismissed] = useState<string | null>(null)

  const refresh = useCallback(async () => {
    try {
      setDetail(await getClientModels(clientId))
      setLoadError(null)
    } catch (err) {
      setLoadError(errorMessage(err))
    }
  }, [clientId])

  // Mounted only while the card is expanded, so this polls only then.
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined
    let stopped = false
    const tick = async () => {
      if (!(config.pause_when_hidden && document.hidden)) await refresh()
      if (!stopped) timer = setTimeout(tick, config.poll_interval_ms)
    }
    void tick()
    return () => {
      stopped = true
      clearTimeout(timer)
    }
  }, [refresh, config.poll_interval_ms, config.pause_when_hidden])

  const canManage = detail?.can_manage ?? false
  useEffect(() => {
    if (!canManage) return
    listCatalog().then(setCatalog, (err: unknown) => console.error(`Couldn't load the catalog: ${errorMessage(err)}`))
  }, [canManage])

  const act = async (action: () => Promise<unknown>) => {
    setActionError(null)
    try {
      await action()
    } catch (err) {
      setActionError(errorMessage(err))
    }
    await refresh()
  }

  if (!detail) {
    return (
      <Typography variant="body2" color={loadError ? 'error' : 'text.secondary'}>
        {loadError ?? 'Loading models…'}
      </Typography>
    )
  }

  const list = rows(detail)
  const installed = new Set(list.filter((r) => r.state !== 'failed').map((r) => r.id))
  const runtime = detail.runtimes.find((r) => r.backend === 'llama_cpp')
  // The newest command, if it failed: why an install didn't happen, e.g. "offline".
  const lastFailure = detail.commands.find((c) => !isCommandActive(c.state))
  const failedRowSaysWhy = (id: string) => list.some((r) => r.id === id && r.state === 'failed' && r.error)
  const showFailure =
    lastFailure &&
    (lastFailure.state === 'failed' || lastFailure.state === 'unsupported') &&
    lastFailure.command_id !== dismissed &&
    !failedRowSaysWhy(lastFailure.model_id)
      ? lastFailure
      : null

  return (
    <Stack spacing={1}>
      <Stack direction="row" sx={{ alignItems: 'center', justifyContent: 'space-between' }}>
        <Typography variant="subtitle2">Models</Typography>
        {canManage && (
          <>
            <Button
              size="small"
              startIcon={<DownloadIcon />}
              disabled={!detail.connected || catalog.length === 0}
              onClick={(e) => setMenu(e.currentTarget)}
              aria-haspopup="menu"
            >
              Install
            </Button>
            <Menu anchorEl={menu} open={menu !== null} onClose={() => setMenu(null)}>
              {catalog.map((entry) => (
                <MenuItem
                  key={entry.id}
                  disabled={installed.has(entry.id)}
                  onClick={() => {
                    setMenu(null)
                    void act(() => installModel(clientId, entry.id))
                  }}
                >
                  <ListItemText
                    primary={entry.display_name}
                    secondary={installed.has(entry.id) ? 'Installed' : formatBytes(entry.size_bytes)}
                  />
                </MenuItem>
              ))}
            </Menu>
          </>
        )}
      </Stack>

      {!detail.connected && (
        <Typography variant="body2" color="text.secondary">
          Disconnected. Its models show when it connects.
        </Typography>
      )}
      {detail.connected && list.length === 0 && (
        <Typography variant="body2" color="text.secondary">
          No models yet.
        </Typography>
      )}

      {list.map((row) => (
        <Box key={row.id}>
          <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
            <Typography variant="body2" sx={{ flex: 1, minWidth: 0, overflowWrap: 'anywhere' }} title={row.id}>
              {row.name}
            </Typography>
            {row.size !== null && (
              <Typography variant="body2" color="text.secondary" sx={{ flexShrink: 0 }}>
                {formatBytes(row.size)}
              </Typography>
            )}
            <Typography
              variant="body2"
              color={row.state === 'failed' ? 'error' : row.state === 'loaded' ? 'success.main' : 'text.secondary'}
              sx={{ flexShrink: 0, minWidth: '5.5rem', textAlign: 'right' }}
            >
              {row.builtIn ? 'Built in' : STATE_LABEL[row.state]}
            </Typography>
            {canManage && !row.builtIn && (
              <Tooltip title={row.pull ? 'Stop the download first' : 'Delete from this client'}>
                <span>
                  <IconButton
                    size="small"
                    aria-label={`Delete ${row.name}`}
                    disabled={row.pull !== null || !detail.connected}
                    onClick={() => {
                      setToDelete(row)
                      setDeleteOpen(true)
                    }}
                  >
                    <DeleteOutlinedIcon fontSize="small" />
                  </IconButton>
                </span>
              </Tooltip>
            )}
          </Stack>
          {row.state === 'downloading' && (
            <Stack direction="row" spacing={1} sx={{ alignItems: 'center', mt: 0.5 }}>
              <Box sx={{ flex: 1 }}>
                <LinearProgress
                  variant={percent(row.pull?.progress ?? null) === undefined ? 'indeterminate' : 'determinate'}
                  value={percent(row.pull?.progress ?? null)}
                  aria-label={`Downloading ${row.name}`}
                />
                <Typography variant="caption" color="text.secondary">
                  {row.pull ? progressText(row.pull.progress) : 'Downloading'}
                </Typography>
              </Box>
              {canManage && row.pull && (
                <Button
                  size="small"
                  color="inherit"
                  onClick={() => {
                    const pull = row.pull
                    if (pull) void act(() => cancelModelCommand(clientId, pull.command_id))
                  }}
                >
                  Cancel
                </Button>
              )}
            </Stack>
          )}
          {row.state === 'failed' && row.error && (
            <Typography variant="caption" color="error" component="div" sx={{ whiteSpace: 'pre-wrap' }}>
              {row.error}
            </Typography>
          )}
        </Box>
      ))}

      {(actionError ?? showFailure) && (
        <Alert
          severity="warning"
          onClose={() => {
            setActionError(null)
            if (showFailure) setDismissed(showFailure.command_id)
          }}
        >
          {actionError ??
            `${showFailure?.kind === 'delete' ? 'Deleting' : 'Installing'} ${showFailure?.model_name}: ${showFailure?.error ?? 'failed'}`}
        </Alert>
      )}

      {detail.storage && (
        <Typography variant="caption" color="text.secondary">
          Models use {formatBytes(detail.storage.models_bytes)}, {formatBytes(detail.storage.free_bytes)} free
        </Typography>
      )}
      {runtime && (
        <Typography variant="caption" color="text.secondary">
          llama.cpp {runtime.version} ({runtime.variant}): {RUNTIME_LABEL[runtime.state]}
        </Typography>
      )}

      <ConfirmationModal
        open={deleteOpen}
        title={`Delete ${toDelete?.name ?? 'model'}?`}
        message={`It's removed from ${clientName}'s disk (unloaded first if it's in use). Installing it again downloads it again.`}
        confirmLabel="Delete"
        confirmColor="error"
        onConfirm={async () => {
          if (!toDelete) return
          await deleteModel(clientId, toDelete.id)
          await refresh()
        }}
        onClose={() => setDeleteOpen(false)}
      />
    </Stack>
  )
}
