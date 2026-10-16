// Display formatting for the hardware specs clients report. The server
// passes raw units through untouched; everything human-readable is here.

import type { Cpu, Disk, Gpu, HardwareSpecs, Memory } from './api'

export const MISSING = '—'

const GIB = 1024 ** 3

function trimNumber(value: number, decimals: number): string {
  return value.toFixed(decimals).replace(/\.?0+$/, '')
}

/** Decimal units, as drive makers use: 512110190592 -> "512 GB". */
export function formatBytes(bytes: number | null | undefined): string {
  if (bytes == null) return MISSING
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB']
  let value = bytes
  let unit = 0
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000
    unit++
  }
  return `${trimNumber(value, value < 100 ? 1 : 0)} ${units[unit]}`
}

// Installed-RAM totals machines actually come in.
const STANDARD_RAM_GB = [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048]

/**
 * System RAM in binary GB. The OS reports less than what's installed
 * (firmware and iGPU reservations), so snap up to the nearest standard
 * size when it's close: 15.4 GiB visible on a 16 GB machine reads "16 GB".
 */
export function formatRamBytes(bytes: number | null | undefined): string {
  if (bytes == null) return MISSING
  const gib = bytes / GIB
  if (gib < 1) return `${Math.round(bytes / 1024 ** 2)} MB`
  const standard = STANDARD_RAM_GB.find((size) => size >= gib - 0.05)
  return `${standard != null && gib >= standard * 0.88 ? standard : Math.round(gib)} GB`
}

/** VRAM in binary GB; drivers report it exactly. */
export function formatVramBytes(bytes: number | null | undefined): string {
  if (bytes == null) return MISSING
  return bytes < GIB ? `${Math.round(bytes / 1024 ** 2)} MB` : `${trimNumber(bytes / GIB, 1)} GB`
}

/** 5700 -> "5.7 GHz", 2520 -> "2.52 GHz", 800 -> "800 MHz". */
export function formatMhz(mhz: number | null | undefined): string {
  if (mhz == null) return MISSING
  return mhz >= 1000 ? `${trimNumber(mhz / 1000, 2)} GHz` : `${mhz} MHz`
}

/** "AMD Ryzen 9 7950X 16-Core Processor" -> "AMD Ryzen 9 7950X". */
export function cleanCpuModel(model: string): string {
  return model
    .replace(/\((R|TM|tm)\)/g, '')
    .replace(/\s+@\s+[\d.]+\s*GHz/i, '')
    .replace(/\b\d+-Core Processor\b/i, '')
    .replace(/\b(CPU|Processor)\b/g, '')
    .replace(/\s+/g, ' ')
    .trim()
}

/** "13th Gen Intel Core i5-13500H" -> "Core i5-13500H". */
function shortCpuModel(model: string): string {
  return cleanCpuModel(model)
    .replace(/^\d+(st|nd|rd|th) Gen /i, '')
    .replace(/^(AMD|Intel) /, '')
}

/** "NVIDIA GeForce RTX 4090" -> "RTX 4090". */
export function shortGpuModel(model: string): string {
  return model.replace(/^(NVIDIA|AMD|Intel(\(R\))?)\s+/, '').replace(/^(GeForce|Radeon)\s+(?=\w)/, '').trim()
}

function joinParts(parts: (string | null | undefined | false)[]): string {
  const present = parts.filter((p): p is string => !!p && p !== MISSING)
  return present.length > 0 ? present.join(' · ') : MISSING
}

/** "Ryzen 9 7950X · 5.7 GHz · 16C/32T" */
export function formatCpu(cpu: Cpu | null): string {
  if (!cpu) return MISSING
  const cores =
    cpu.physical_cores != null && cpu.logical_cores != null
      ? `${cpu.physical_cores}C/${cpu.logical_cores}T`
      : cpu.logical_cores != null
        ? `${cpu.logical_cores} threads`
        : null
  return joinParts([cpu.model && cleanCpuModel(cpu.model), cpu.max_mhz != null && formatMhz(cpu.max_mhz), cores])
}

/** "64 GB DDR5-6000 (2 modules)" */
export function formatMemory(memory: Memory | null): string {
  if (!memory) return MISSING
  const kind = memory.standard
    ? memory.speed_mts
      ? `${memory.standard}-${memory.speed_mts}`
      : memory.standard
    : null
  const modules = memory.module_count != null ? `(${memory.module_count} module${memory.module_count === 1 ? '' : 's'})` : null
  const parts = [memory.total_bytes != null ? formatRamBytes(memory.total_bytes) : null, kind, modules].filter(Boolean)
  return parts.length > 0 ? parts.join(' ') : MISSING
}

/** "RTX 4090 · 24 GB · 2.52 GHz", or "Iris Xe Graphics · shared memory". */
export function formatGpu(gpu: Gpu): string {
  const memory = gpu.vram_shared ? 'shared memory' : gpu.vram_bytes != null ? formatVramBytes(gpu.vram_bytes) : null
  return joinParts([
    gpu.model && shortGpuModel(gpu.model),
    memory,
    gpu.max_core_mhz != null && formatMhz(gpu.max_core_mhz),
  ])
}

const INTERFACE_LABELS: Record<Disk['interface'], string | null> = {
  nvme: 'NVMe',
  sata: 'SATA',
  sas: 'SAS',
  usb: 'USB',
  virtual: 'Virtual',
  other: null,
}

const MEDIA_LABELS: Record<Disk['media'], string | null> = { ssd: 'SSD', hdd: 'HDD', unknown: null }

/** "Samsung 990 Pro · NVMe SSD · 1.2 TB free of 2 TB" */
export function formatDisk(disk: Disk): string {
  const kind = [INTERFACE_LABELS[disk.interface], MEDIA_LABELS[disk.media]].filter(Boolean).join(' ')
  const size =
    disk.total_bytes == null
      ? null
      : disk.available_bytes != null
        ? `${formatBytes(disk.available_bytes)} free of ${formatBytes(disk.total_bytes)}`
        : formatBytes(disk.total_bytes)
  return joinParts([disk.model, kind, size])
}

/**
 * One line for the card header: the main GPU (or the CPU, on machines
 * with only integrated graphics) and RAM, e.g. "RTX 4090 · 64 GB DDR5".
 */
export function specsSummary(specs: HardwareSpecs | null): string | null {
  if (!specs) return null
  const gpu = specs.gpus.find((g) => !g.vram_shared && g.model)
  const lead = gpu?.model ? shortGpuModel(gpu.model) : specs.cpu?.model ? shortCpuModel(specs.cpu.model) : null
  const memory = specs.memory
  const ram =
    memory?.total_bytes != null
      ? [formatRamBytes(memory.total_bytes), memory.standard].filter(Boolean).join(' ')
      : null
  const summary = [lead, ram].filter(Boolean).join(' · ')
  return summary || null
}
