import Box from '@mui/material/Box'
import Typography from '@mui/material/Typography'
import { memo } from 'react'

export interface UsageGraphProps {
  /** 0–100, oldest first, at most `capacity` long. `null` is a gap (no data for that slot). */
  values: readonly (number | null)[]
  /** Slots across the graph: METRICS_HISTORY_SAMPLES. */
  capacity: number
  /**
   * How many slots this graph has advanced in total. The vertical grid lines
   * shift left by one slot's width per step, so the grid scrolls with the data.
   */
  scrollOffset: number
  width: number
  height: number
  gridPx: number
  lineColor: string
  gridColor: string
  bgColor: string
  caption: string
  ariaLabel: string
  /** No data (disconnected, or no samples yet): the screen is drawn darker. */
  dimmed?: boolean
}

/**
 * A Task Manager-style usage history: dark screen, green grid, one bright
 * line, newest value at the right edge. The screen keeps its own colours in
 * both themes (it's meant to look like a screen); the border and caption
 * follow the MUI theme. Plain SVG, and memoised, since every card redraws
 * several of these each second.
 */
function UsageGraph({
  values,
  capacity,
  scrollOffset,
  width,
  height,
  gridPx,
  lineColor,
  gridColor,
  bgColor,
  caption,
  ariaLabel,
  dimmed = false,
}: UsageGraphProps) {
  const step = width / Math.max(1, capacity - 1)
  // A buffer that isn't full yet starts partway across and ends at the right edge.
  const firstSlot = capacity - values.length
  // 0% at the bottom, 100% at the top, inset 1px so either extreme stays visible.
  const yOf = (v: number) => 1 + (1 - Math.min(100, Math.max(0, v)) / 100) * (height - 2)

  let path = ''
  let segment = 0 // points in the current run between gaps
  values.forEach((v, i) => {
    if (v == null || !Number.isFinite(v)) {
      segment = 0
      return
    }
    const point = `${((firstSlot + i) * step).toFixed(2)} ${yOf(v).toFixed(2)}`
    path += segment === 0 ? `M${point}` : `L${point}`
    // A lone point between gaps is a zero-length line, which round caps
    // turn into a dot; a bare moveto would draw nothing.
    if (segment === 0 && (i === values.length - 1 || values[i + 1] == null)) path += `L${point}`
    segment++
  })

  const horizontal: number[] = []
  for (let y = gridPx; y < height; y += gridPx) horizontal.push(y)
  // Each new sample moves the data one step left; the vertical lines move
  // with it, wrapping every gridPx.
  const phase = (((scrollOffset * step) % gridPx) + gridPx) % gridPx
  const vertical: number[] = []
  for (let x = gridPx - phase; x < width; x += gridPx) if (x > 0) vertical.push(x)

  return (
    // The caption may run wider than the graph rather than be cut off.
    <Box sx={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-start', gap: 0.5, minWidth: width + 2 }}>
      <Box sx={{ border: 1, borderColor: 'divider', lineHeight: 0 }}>
        <svg
          role="img"
          aria-label={ariaLabel}
          width={width}
          height={height}
          viewBox={`0 0 ${width} ${height}`}
          // Darkened rather than faded, so it looks the same on light and
          // dark cards: a screen with nothing on it.
          style={{ display: 'block', filter: dimmed ? 'brightness(0.5)' : undefined }}
        >
          <rect width={width} height={height} fill={bgColor} />
          <g stroke={gridColor} strokeWidth={1} shapeRendering="crispEdges">
            {horizontal.map((y) => (
              <line key={`h${y}`} x1={0} x2={width} y1={y} y2={y} />
            ))}
            {vertical.map((x, i) => (
              <line key={`v${i}`} x1={x} x2={x} y1={0} y2={height} />
            ))}
          </g>
          {path && (
            <path
              d={path}
              fill="none"
              stroke={lineColor}
              strokeWidth={1.5}
              strokeLinejoin="round"
              strokeLinecap="round"
            />
          )}
        </svg>
      </Box>
      <Typography
        variant="caption"
        color="text.secondary"
        sx={{ lineHeight: 1.3, whiteSpace: 'nowrap', fontVariantNumeric: 'tabular-nums' }}
      >
        {caption}
      </Typography>
    </Box>
  )
}

export default memo(UsageGraph)
