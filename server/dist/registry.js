// Deliberately dumb, file-backed storage for the sanity-check scaffold.
// The real build replaces this with SQLite (see the main proposal), but a
// database dependency isn't worth adding yet while we're still proving the
// networking path works at all — and it sidesteps a real risk: native
// modules (anything needing node-gyp, e.g. better-sqlite3) are painful to
// get running under Alpine's musl libc, which is what the Pi will run.
// Keeping the scaffold on pure-JS dependencies (@noble/ed25519, ws) means
// `npm install` on the actual Pi should "just work" without a native build
// step. Worth re-validating this assumption once you're actually on the Pi.
import { promises as fs } from "node:fs";
import path from "node:path";
const DATA_DIR = path.join(process.cwd(), "data");
const CLIENTS_FILE = path.join(DATA_DIR, "clients.json");
const CODES_FILE = path.join(DATA_DIR, "pending-codes.json");
async function ensureDataDir() {
    await fs.mkdir(DATA_DIR, { recursive: true });
}
async function readJson(file, fallback) {
    try {
        const raw = await fs.readFile(file, "utf-8");
        return JSON.parse(raw);
    }
    catch (err) {
        if (err.code === "ENOENT")
            return fallback;
        throw err;
    }
}
async function writeJson(file, data) {
    await ensureDataDir();
    // Write to a temp file then rename, so a crash mid-write can't corrupt
    // the registry the way a direct write could.
    const tmp = `${file}.tmp`;
    await fs.writeFile(tmp, JSON.stringify(data, null, 2));
    await fs.rename(tmp, file);
}
// ---- pairing codes ----
export async function createPairingCode(ttlMs = 10 * 60 * 1000) {
    const code = Math.random().toString(36).slice(2, 10).toUpperCase();
    const codes = await readJson(CODES_FILE, {});
    codes[code] = { expires_at: Date.now() + ttlMs };
    await writeJson(CODES_FILE, codes);
    return code;
}
export async function consumePairingCode(code) {
    const codes = await readJson(CODES_FILE, {});
    const entry = codes[code];
    if (!entry)
        return false;
    const valid = entry.expires_at > Date.now();
    delete codes[code]; // one-time use regardless of outcome
    await writeJson(CODES_FILE, codes);
    return valid;
}
// ---- clients ----
export async function addClient(record) {
    const clients = await readJson(CLIENTS_FILE, {});
    clients[record.client_id] = record;
    await writeJson(CLIENTS_FILE, clients);
}
export async function getClient(clientId) {
    const clients = await readJson(CLIENTS_FILE, {});
    return clients[clientId];
}
