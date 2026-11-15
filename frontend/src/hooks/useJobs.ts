import { useEffect, useState } from 'react'
import { errorMessage, listJobs, type DashboardConfig, type Job } from '../api'

/** How many finished jobs the page shows. */
export const RECENT_JOBS = 50
/** More active jobs than this can't exist: the queue is capped well below it. */
const ACTIVE_JOBS_LIMIT = 200

export interface Jobs {
  /** Queued, assigned or running; oldest in the queue first. */
  active: Job[]
  /** Finished; newest first. */
  recent: Job[]
}

/**
 * Polls `GET /api/jobs` the way the clients page polls `/api/clients`: every
 * `poll_interval_ms`, each poll scheduled only once the previous one has
 * finished, and paused while the tab is hidden when the server says so.
 * `everyone` is the admin's view of all users' jobs. Nothing runs until
 * `config` is loaded.
 */
export function useJobs(config: DashboardConfig | null, everyone: boolean) {
  const [jobs, setJobs] = useState<Jobs | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!config) return
    let stopped = false
    let timer: ReturnType<typeof setTimeout> | undefined

    const poll = async () => {
      try {
        const [active, recent] = await Promise.all([
          listJobs('active', ACTIVE_JOBS_LIMIT, everyone),
          listJobs('recent', RECENT_JOBS, everyone),
        ])
        if (stopped) return
        setJobs({ active, recent })
        setError(null)
      } catch (err) {
        if (!stopped) setError(errorMessage(err))
      }
    }

    const paused = () => config.pause_when_hidden && document.visibilityState === 'hidden'
    const schedule = () => {
      clearTimeout(timer)
      if (stopped || paused()) return
      timer = setTimeout(() => void poll().then(schedule), config.poll_interval_ms)
    }
    // Back from hidden: poll at once.
    const onVisibilityChange = () => {
      if (!config.pause_when_hidden) return
      clearTimeout(timer)
      if (!paused()) void poll().then(schedule)
    }

    document.addEventListener('visibilitychange', onVisibilityChange)
    void poll().then(schedule)
    return () => {
      stopped = true
      clearTimeout(timer)
      document.removeEventListener('visibilitychange', onVisibilityChange)
    }
  }, [config, everyone])

  return { jobs, error }
}
