// Tiny standalone admin script — not exposed over HTTP on purpose, since
// this scaffold has no dashboard/auth for admin actions yet. Run this on
// the server machine itself to mint a pairing code, then hand that code
// to whoever is enrolling a new client agent.
import { createPairingCode } from "./registry.js";

const code = await createPairingCode();
console.log(`Pairing code (valid 10 minutes): ${code}`);
