import { useEffect, useState } from 'react'
import { DEFAULT_DASHBOARD_CONFIG, errorMessage, getDashboardConfig, type DashboardConfig } from '../api'

/**
 * The server's DASHBOARD_* settings, fetched once. Null until loaded; if
 * that fails, the built-in defaults (the same values the server defaults to).
 */
export function useDashboardConfig(): DashboardConfig | null {
  const [config, setConfig] = useState<DashboardConfig | null>(null)
  useEffect(() => {
    let cancelled = false
    getDashboardConfig().then(
      (loaded) => {
        if (!cancelled) setConfig(loaded)
      },
      (err: unknown) => {
        console.error(`Couldn't load /api/dashboard-config, using defaults: ${errorMessage(err)}`)
        if (!cancelled) setConfig(DEFAULT_DASHBOARD_CONFIG)
      },
    )
    return () => {
      cancelled = true
    }
  }, [])
  return config
}
