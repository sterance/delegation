import Box from '@mui/material/Box'
import type { SxProps, Theme } from '@mui/material/styles'
import { useMemo } from 'react'
import type { DashboardConfig, Sample } from '../api'
import { NO_VALUE, formatGib } from '../format'
import { latestSample, type History } from '../useClients'
import UsageGraph from './UsageGraph'

interface GraphSpec {
  key: string
  values: (number | null)[]
  caption: string
  ariaLabel: string
}

function percent(used: number, total: number): number | null {
  return total > 0 ? (used / total) * 100 : null
}

function series(slots: History['slots'], pick: (s: Sample) => number | null | undefined): (number | null)[] {
  return slots.map((s) => (s ? (pick(s) ?? null) : null))
}

function cpuGraph(slots: History['slots'], latest: Sample | null): GraphSpec {
  const cpu = latest ? Math.round(latest.cpu_percent) : null
  return {
    key: 'cpu',
    values: series(slots, (s) => s.cpu_percent),
    caption: `CPU ${cpu != null ? `${cpu}%` : NO_VALUE}`,
    ariaLabel: cpu != null ? `CPU usage, ${cpu} percent` : 'CPU usage, no data',
  }
}

function ramGraph(slots: History['slots'], latest: Sample | null): GraphSpec {
  const values = series(slots, (s) => percent(s.ram_total_bytes - s.ram_available_bytes, s.ram_total_bytes))
  if (!latest) return { key: 'ram', values, caption: `RAM ${NO_VALUE}`, ariaLabel: 'Memory usage, no data' }
  const used = latest.ram_total_bytes - latest.ram_available_bytes
  const pct = Math.round(percent(used, latest.ram_total_bytes) ?? 0)
  const amounts = `${formatGib(used)} / ${formatGib(latest.ram_total_bytes)} GiB`
  return {
    key: 'ram',
    values,
    caption: `RAM ${amounts}`,
    ariaLabel: `Memory usage, ${pct} percent, ${amounts.replace(' / ', ' of ')}`,
  }
}

function gpuGraphs(slots: History['slots'], latest: Sample | null, i: number): GraphSpec[] {
  const gpu = latest?.gpus[i]
  const util = series(slots, (s) => s.gpus[i]?.utilisation_percent)
  const vram = series(slots, (s) => {
    const g = s.gpus[i]
    return g ? percent(g.vram_used_bytes, g.vram_total_bytes) : null
  })
  if (!gpu) {
    return [
      { key: `gpu${i}`, values: util, caption: `GPU${i} ${NO_VALUE}`, ariaLabel: `GPU ${i} usage, no data` },
      { key: `vram${i}`, values: vram, caption: `VRAM${i} ${NO_VALUE}`, ariaLabel: `GPU ${i} memory usage, no data` },
    ]
  }
  const pct = Math.round(gpu.utilisation_percent)
  const watts = gpu.power_watts != null ? Math.round(gpu.power_watts) : null
  const vramPct = Math.round(percent(gpu.vram_used_bytes, gpu.vram_total_bytes) ?? 0)
  const amounts = `${formatGib(gpu.vram_used_bytes)} / ${formatGib(gpu.vram_total_bytes)} GiB`
  return [
    {
      key: `gpu${i}`,
      values: util,
      caption: `GPU${i} ${pct}%${watts != null ? ` · ${watts} W` : ''}`,
      ariaLabel: `GPU ${i} (${gpu.name}) usage, ${pct} percent${watts != null ? `, ${watts} watts` : ''}`,
    },
    {
      key: `vram${i}`,
      values: vram,
      caption: `VRAM${i} ${amounts}`,
      ariaLabel: `GPU ${i} memory usage, ${vramPct} percent, ${amounts.replace(' / ', ' of ')}`,
    },
  ]
}

interface Props {
  history: History | undefined
  config: DashboardConfig
  /** Include the CPU and RAM graphs. */
  system: boolean
  /** GPU indexes to graph: `gpuFrom` up to (not including) `gpuTo`. */
  gpuFrom: number
  gpuTo: number
  sx?: SxProps<Theme>
}

/** A wrapping row of usage graphs for one client. */
export default function ClientUsage({ history, config, system, gpuFrom, gpuTo, sx }: Props) {
  const graphs = useMemo(() => {
    const slots = history?.slots ?? []
    const latest = latestSample(history)
    const specs: GraphSpec[] = system ? [cpuGraph(slots, latest), ramGraph(slots, latest)] : []
    for (let i = gpuFrom; i < gpuTo; i++) specs.push(...gpuGraphs(slots, latest, i))
    return { specs, dimmed: !latest }
  }, [history, system, gpuFrom, gpuTo])

  if (graphs.specs.length === 0) return null
  return (
    <Box sx={[{ display: 'flex', flexWrap: 'wrap', gap: 1.5, minWidth: 0 }, ...(Array.isArray(sx) ? sx : [sx])]}>
      {graphs.specs.map((g) => (
        <UsageGraph
          key={g.key}
          values={g.values}
          capacity={config.metrics_history_samples}
          scrollOffset={history?.advanced ?? 0}
          width={config.graph_width_px}
          height={config.graph_height_px}
          gridPx={config.graph_grid_px}
          lineColor={config.graph_line_color}
          gridColor={config.graph_grid_color}
          bgColor={config.graph_bg_color}
          caption={g.caption}
          ariaLabel={g.ariaLabel}
          dimmed={graphs.dimmed}
        />
      ))}
    </Box>
  )
}
