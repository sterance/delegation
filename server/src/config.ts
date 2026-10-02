function durationMs(name: string, fallback: number): number {
  const raw = process.env[name];
  if (raw === undefined) return fallback;

  const value = Number(raw);
  if (!Number.isInteger(value) || value <= 0) {
    throw new Error(`${name} must be a positive integer number of milliseconds; got "${raw}"`);
  }
  return value;
}

export const AUTH_TIMEOUT_MS = durationMs("SERVER_AUTH_TIMEOUT_MS", 5_000);
export const HEARTBEAT_TIMEOUT_MS = durationMs("SERVER_HEARTBEAT_TIMEOUT_MS", 30_000);
export const HEARTBEAT_SWEEP_INTERVAL_MS = durationMs("SERVER_HEARTBEAT_SWEEP_INTERVAL_MS", 10_000);
export const PAIRING_CODE_TTL_MS = durationMs("SERVER_PAIRING_CODE_TTL_MS", 10 * 60 * 1_000);
