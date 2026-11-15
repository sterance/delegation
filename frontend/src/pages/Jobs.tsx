import Alert from '@mui/material/Alert'
import Box from '@mui/material/Box'
import Chip from '@mui/material/Chip'
import CircularProgress from '@mui/material/CircularProgress'
import Container from '@mui/material/Container'
import FormControlLabel from '@mui/material/FormControlLabel'
import Link from '@mui/material/Link'
import Paper from '@mui/material/Paper'
import Stack from '@mui/material/Stack'
import Switch from '@mui/material/Switch'
import Table from '@mui/material/Table'
import TableBody from '@mui/material/TableBody'
import TableCell from '@mui/material/TableCell'
import TableContainer from '@mui/material/TableContainer'
import TableHead from '@mui/material/TableHead'
import TableRow from '@mui/material/TableRow'
import Tooltip from '@mui/material/Tooltip'
import Typography from '@mui/material/Typography'
import { useState } from 'react'
import { Link as RouterLink } from 'react-router'
import type { Job, JobAttempt } from '../api'
import { useAuth } from '../auth/context'
import { useDashboardConfig } from '../hooks/useDashboardConfig'
import { RECENT_JOBS, useJobs } from '../hooks/useJobs'
import type { MessageStatus } from '../types/chat'
import { formatElapsed } from '../utils/format'

const STATUS_COLOR: Record<MessageStatus, 'default' | 'info' | 'success' | 'error' | 'warning'> = {
  queued: 'default',
  assigned: 'info',
  running: 'info',
  completed: 'success',
  failed: 'error',
  cancelled: 'default',
  orphaned: 'warning',
}

const KIND_LABEL: Record<Job['kind'], string> = { chat_reply: 'Chat reply' }

/** "1. alice-pc: lost" for each attempt, oldest first. */
function attemptText(attempt: JobAttempt): string {
  return `${attempt.number}. ${attempt.client_label}: ${(attempt.outcome ?? 'running').replace('_', ' ')}`
}

function JobRow({ job, showOwner }: { job: Job; showOwner: boolean }) {
  const target = job.target.kind === 'pool' ? 'Any client' : (job.target.client_label ?? 'A deleted client')
  return (
    <TableRow hover>
      <TableCell>{KIND_LABEL[job.kind] ?? job.kind}</TableCell>
      {showOwner && <TableCell>{job.owner.username}</TableCell>}
      <TableCell>
        <Tooltip title={job.error ?? ''}>
          <Chip size="small" label={job.status} color={STATUS_COLOR[job.status]} variant="outlined" />
        </Tooltip>
      </TableCell>
      <TableCell>{job.model}</TableCell>
      <TableCell>{target}</TableCell>
      <TableCell align="right">
        {job.attempts} of {job.max_attempts}
      </TableCell>
      <TableCell align="right">{formatElapsed(job.queue_wait_ms)}</TableCell>
      <TableCell align="right">{formatElapsed(job.run_ms)}</TableCell>
      <TableCell>
        {job.history.length === 0 ? (
          <Typography variant="body2" color="text.secondary">
            Not yet
          </Typography>
        ) : (
          job.history.map((attempt) => (
            <Tooltip key={attempt.id} title={attempt.error ?? ''} placement="left">
              <Typography variant="body2" noWrap>
                {attemptText(attempt)}
              </Typography>
            </Tooltip>
          ))
        )}
      </TableCell>
      <TableCell>
        {job.conversation_id && (
          <Link component={RouterLink} to={`/chat?c=${encodeURIComponent(job.conversation_id)}`} underline="hover">
            Open
          </Link>
        )}
      </TableCell>
    </TableRow>
  )
}

function JobTable({ title, jobs, empty, showOwner }: { title: string; jobs: Job[]; empty: string; showOwner: boolean }) {
  return (
    <Box>
      <Typography variant="subtitle1" component="h3" sx={{ mb: 1 }}>
        {title} ({jobs.length})
      </Typography>
      {jobs.length === 0 ? (
        <Typography variant="body2" color="text.secondary">
          {empty}
        </Typography>
      ) : (
        <TableContainer component={Paper} variant="outlined">
          <Table size="small">
            <TableHead>
              <TableRow>
                <TableCell>Kind</TableCell>
                {showOwner && <TableCell>Owner</TableCell>}
                <TableCell>Status</TableCell>
                <TableCell>Model</TableCell>
                <TableCell>Target</TableCell>
                <TableCell align="right">Attempts</TableCell>
                <TableCell align="right">Queue wait</TableCell>
                <TableCell align="right">Run time</TableCell>
                <TableCell>Ran on</TableCell>
                <TableCell>Chat</TableCell>
              </TableRow>
            </TableHead>
            <TableBody>
              {jobs.map((job) => (
                <JobRow key={job.id} job={job} showOwner={showOwner} />
              ))}
            </TableBody>
          </Table>
        </TableContainer>
      )}
    </Box>
  )
}

/** The job queue: what's waiting or running now, and what finished lately. */
export default function Jobs() {
  const config = useDashboardConfig()
  const isAdmin = useAuth().user?.role === 'admin'
  const [everyone, setEveryone] = useState(false)
  const showAll = isAdmin && everyone
  const { jobs, error } = useJobs(config, showAll)

  return (
    <Container maxWidth="xl" sx={{ py: 4 }}>
      <Stack direction="row" spacing={2} sx={{ alignItems: 'center', justifyContent: 'space-between', mb: 2 }}>
        <Box>
          <Typography variant="h5" component="h2">
            Jobs
          </Typography>
          <Typography variant="body2" color="text.secondary">
            Chat replies waiting for a client or being generated, and the last {RECENT_JOBS} that finished.
          </Typography>
        </Box>
        {isAdmin && (
          <FormControlLabel
            control={<Switch checked={everyone} onChange={(e) => setEveryone(e.target.checked)} />}
            label="Everyone's"
          />
        )}
      </Stack>

      {error && (
        <Alert severity={jobs ? 'warning' : 'error'} sx={{ mb: 2 }}>
          {jobs
            ? `Couldn't refresh the jobs (${error}). What's below may be out of date.`
            : `Couldn't load the jobs (${error}). Retrying…`}
        </Alert>
      )}

      {jobs ? (
        <Stack spacing={4}>
          <JobTable title="Active" jobs={jobs.active} empty="Nothing is queued or running." showOwner={showAll} />
          <JobTable title="Finished" jobs={jobs.recent} empty="No jobs have finished yet." showOwner={showAll} />
        </Stack>
      ) : (
        !error && (
          <Box sx={{ display: 'flex', justifyContent: 'center', py: 6 }}>
            <CircularProgress aria-label="Loading jobs" />
          </Box>
        )
      )}
    </Container>
  )
}
