// Tiny standalone admin script — not exposed over HTTP on purpose, since
// this scaffold has no dashboard/auth for admin actions yet. Run this on
// the server machine itself to mint a pairing code, then hand that code
// to whoever is enrolling a new client agent.
import { createPairingCode } from "./registry.js";
import { PAIRING_CODE_TTL_MS } from "./config.js";

const code = await createPairingCode();
console.log(`[${new Date().toISOString()}] Pairing code (valid ${PAIRING_CODE_TTL_MS / 60_000} minutes): ${code}`);
