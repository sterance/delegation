import { useCallback, useEffect, useRef, useState } from 'react'
import { errorMessage, listClients, type Client, type DashboardConfig, type HardwareSpecs, type Sample } from './api'

/** One client's graph data, built up from successive polls. */
export interface History {
  /**
   * Oldest first, at most METRICS_HISTORY_SAMPLES long; one slot per tick.
   * `null` marks ticks that arrived without a sample (the client's sampler
   * stalled, or ticks were lost), so the graphs show a gap rather than
   * squeezing time together.
   */
  slots: readonly (Sample | null)[]
  /** Slots appended since this page started following the client; scrolls the graph grid. */
  advanced: number
}

const EMPTY: History = { slots: [], advanced: 0 }

/** The newest sample in a history, if there is one. */
export function latestSample(history: History | undefined): Sample | null {
  const slots = history?.slots ?? []
  for (let i = slots.length - 1; i >= 0; i--) {
    const slot = slots[i]
    if (slot) return slot
  }
  return null
}

/**
 * How many GPUs a client reports metrics for. Before it has any samples
 * (disconnected, or just connected), guess from its hardware specs so the
 * card keeps the same shape: metrics only ever cover NVIDIA GPUs.
 */
export function metricsGpuCount(history: History | undefined, specs: HardwareSpecs | null): number {
  const latest = latestSample(history)
  if (latest) return latest.gpus.length
  return specs?.gpus.filter((g) => g.vendor === 'NVIDIA').length ?? 0
}

/**
 * Appends samples not already held, inserting a gap for each tick missing
 * between consecutive samples, and trims to `capacity`. Returns `prev`
 * itself when nothing changed, so memoised graphs skip re-rendering.
 */
export function appendSamples(
  prev: History | undefined,
  samples: readonly Sample[],
  capacity: number,
  tickMs: number,
): History {
  let last = latestSample(prev)
  const fresh = samples.filter((s) => !last || s.seq > last.seq)
  if (fresh.length === 0) return prev ?? EMPTY

  const slots = [...(prev?.slots ?? [])]
  let advanced = prev?.advanced ?? 0
  for (const sample of fresh) {
    if (last) {
      // Ticks are periodic, so a gap of about n ticks between receipts means
      // n - 1 of them carried nothing. Rounding absorbs network jitter.
      const gapMs = Date.parse(sample.received_at) - Date.parse(last.received_at)
      const missing = Math.min(capacity, Math.max(0, Math.round(gapMs / tickMs) - 1))
      for (let i = 0; i < missing; i++) slots.push(null)
      advanced += missing
    }
    slots.push(sample)
    advanced++
    last = sample
  }
  return { slots: slots.slice(-capacity), advanced }
}

interface State {
  clients: Client[] | null
  /** Connected clients only: a client's history is dropped as soon as it shows as disconnected. */
  history: Record<string, History>
}

/**
 * Polls `GET /clients?since=<cursor>` every `poll_interval_ms`, each poll
 * scheduled only once the previous one has finished, and keeps each
 * client's sample history. `refresh` polls immediately and resolves once
 * data fetched after the call has been applied. Nothing runs until
 * `config` is loaded.
 */
export function useClients(config: DashboardConfig | null) {
  const [state, setState] = useState<State>({ clients: null, history: {} })
  const [error, setError] = useState<string | null>(null)
  // Shared across effect runs (React StrictMode mounts twice in dev), so a
  // remount carries on from where the last poll left off.
  const cursor = useRef<{ epoch: string | null; seq: number }>({ epoch: null, seq: 0 })
  const history = useRef<Record<string, History>>({})
  const pollNow = useRef<(() => Promise<void>) | null>(null)

  const refresh = useCallback(() => pollNow.current?.() ?? Promise.resolve(), [])

  useEffect(() => {
    if (!config) return
    let stopped = false
    let timer: ReturnType<typeof setTimeout> | undefined
    let running: Promise<void> | null = null
    let again = false

    const fetchAndApply = async () => {
      for (;;) {
        const { epoch, seq } = cursor.current
        const res = await listClients(epoch === null ? 0 : seq)
        if (stopped) return
        if (epoch !== null && (res.epoch !== epoch || res.latest_seq < seq)) {
          // The server restarted: its sequence numbers began again, so this
          // response may have skipped samples. Start over from its full history.
          cursor.current = { epoch: res.epoch, seq: 0 }
          history.current = {}
          continue
        }
        const next: Record<string, History> = {}
        for (const client of res.clients) {
          if (!client.connected) continue
          next[client.client_id] = appendSamples(
            history.current[client.client_id],
            client.samples,
            config.metrics_history_samples,
            config.tick_interval_ms,
          )
        }
        history.current = next
        cursor.current = { epoch: res.epoch, seq: res.latest_seq }
        setState({ clients: res.clients, history: next })
        setError(null)
        return
      }
    }

    // One request at a time, so the cursor only ever moves forward. A poll
    // asked for while one is running runs again straight after it.
    const poll = (): Promise<void> => {
      if (running) {
        again = true
        return running
      }
      running = (async () => {
        do {
          again = false
          try {
            await fetchAndApply()
          } catch (err) {
            if (!stopped) setError(errorMessage(err))
          }
        } while (again && !stopped)
      })().finally(() => {
        running = null
      })
      return running
    }

    const paused = () => config.pause_when_hidden && document.visibilityState === 'hidden'
    const schedule = () => {
      clearTimeout(timer)
      if (stopped || paused()) return
      timer = setTimeout(() => void poll().then(schedule), config.poll_interval_ms)
    }
    // Back from hidden: poll at once. The cursor fills the gap from the
    // server's history.
    const onVisibilityChange = () => {
      if (!config.pause_when_hidden) return
      clearTimeout(timer)
      if (!paused()) void poll().then(schedule)
    }

    pollNow.current = poll
    document.addEventListener('visibilitychange', onVisibilityChange)
    void poll().then(schedule)
    return () => {
      stopped = true
      clearTimeout(timer)
      document.removeEventListener('visibilitychange', onVisibilityChange)
      if (pollNow.current === poll) pollNow.current = null
    }
  }, [config])

  return { clients: state.clients, history: state.history, error, refresh }
}
