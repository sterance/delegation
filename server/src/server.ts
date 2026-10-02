import http from "node:http";
import crypto from "node:crypto";
import { WebSocketServer, WebSocket } from "ws";
import * as ed from "@noble/ed25519";
import type { ClientMessage, ServerMessage, EnrollRequest } from "./protocol.js";
import { addClient, consumePairingCode, getClient } from "./registry.js";
import { AUTH_TIMEOUT_MS, HEARTBEAT_SWEEP_INTERVAL_MS, HEARTBEAT_TIMEOUT_MS } from "./config.js";

// Hurdle #1 encountered while building this: @noble/ed25519 v2's docs (and
// a lot of copy-pasted example code online) show manually wiring up
// `ed.hashes.sha512 = sha512` from @noble/hashes. That API doesn't exist in
// the 2.3.0 that actually installs today — this build ships a working
// `etc.sha512Async` (backed by Node's built-in `node:crypto`) out of the
// box, so no extra dependency is needed at all. Worth re-checking against
// whatever version is current when you actually build this for real,
// since it clearly moved once already.

const PORT = Number(process.env.SERVER_PORT ?? 7070);

function b64ToBytes(b64: string): Uint8Array {
  return new Uint8Array(Buffer.from(b64, "base64"));
}

function log(...args: unknown[]) {
  console.log(`[${new Date().toISOString()}]`, ...args);
}

function logErr(...args: unknown[]) {
  console.error(`[${new Date().toISOString()}]`, ...args);
}

function send(ws: WebSocket, msg: ServerMessage) {
  ws.send(JSON.stringify(msg));
}

// ---------------- HTTP: enrollment ----------------

const httpServer = http.createServer((req, res) => {
  if (req.method === "POST" && req.url === "/enroll") {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", async () => {
      try {
        const parsed = JSON.parse(body) as EnrollRequest;
        const ok = await consumePairingCode(parsed.pairing_code);
        if (!ok) {
          res.writeHead(403, { "content-type": "application/json" });
          res.end(JSON.stringify({ error: "invalid or expired pairing code" }));
          return;
        }
        const client_id = crypto.randomUUID();
        await addClient({
          client_id,
          public_key: parsed.public_key,
          hostname: parsed.hostname,
          enrolled_at: new Date().toISOString(),
        });
        log(`[enroll] new client ${client_id} (${parsed.hostname})`);
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ client_id }));
      } catch (err) {
        logErr("[enroll] error", err);
        res.writeHead(400, { "content-type": "application/json" });
        res.end(JSON.stringify({ error: "bad request" }));
      }
    });
    return;
  }

  res.writeHead(404);
  res.end();
});

// ---------------- WebSocket: challenge-response + heartbeat ----------------

type ConnState = { stage: "awaiting_hello" } | { stage: "awaiting_response"; client_id: string; nonce: Uint8Array } | { stage: "authenticated"; client_id: string; lastHeartbeat: number };

const wss = new WebSocketServer({ server: httpServer, path: "/agent" });

// Tracked outside the per-connection closure so the sweep below can see it.
const connections = new Map<WebSocket, ConnState>();

wss.on("connection", (ws) => {
  let state: ConnState = { stage: "awaiting_hello" };
  connections.set(ws, state);
  const setState = (s: ConnState) => {
    state = s;
    connections.set(ws, s);
  };

  const authTimeout = setTimeout(() => {
    if (state.stage !== "authenticated") {
      log("[ws] handshake timed out, closing");
      ws.close(4001, "handshake timeout");
    }
  }, AUTH_TIMEOUT_MS);

  ws.on("message", async (raw) => {
    let msg: ClientMessage;
    try {
      msg = JSON.parse(raw.toString());
    } catch {
      ws.close(4000, "invalid json");
      return;
    }

    if (msg.type === "hello" && state.stage === "awaiting_hello") {
      const record = await getClient(msg.client_id);
      if (!record) {
        log(`[ws] unknown client_id ${msg.client_id}`);
        send(ws, { type: "auth_failed", reason: "unknown client_id" });
        ws.close(4003, "unknown client");
        return;
      }
      const nonce = crypto.randomBytes(16);
      setState({ stage: "awaiting_response", client_id: msg.client_id, nonce });
      send(ws, { type: "challenge", nonce: nonce.toString("base64") });
      return;
    }

    if (msg.type === "challenge_response" && state.stage === "awaiting_response") {
      const record = await getClient(state.client_id);
      if (!record) {
        ws.close(4003, "unknown client");
        return;
      }
      const publicKey = b64ToBytes(record.public_key);
      const signature = b64ToBytes(msg.signature);
      const valid = await ed.verifyAsync(signature, state.nonce, publicKey);
      if (!valid) {
        log(`[ws] bad signature from ${state.client_id}`);
        send(ws, { type: "auth_failed", reason: "signature verification failed" });
        ws.close(4003, "auth failed");
        return;
      }
      clearTimeout(authTimeout);
      log(`[ws] client ${state.client_id} authenticated`);
      setState({ stage: "authenticated", client_id: state.client_id, lastHeartbeat: Date.now() });
      send(ws, { type: "auth_ok" });
      return;
    }

    if (msg.type === "heartbeat" && state.stage === "authenticated") {
      state.lastHeartbeat = Date.now();
      log(`[heartbeat] ${state.client_id} (${msg.hostname}) at ${new Date(msg.timestamp).toISOString()}`);
      send(ws, { type: "heartbeat_ack" });
      return;
    }

    log(`[ws] unexpected message "${msg.type}" in stage "${state.stage}"`);
  });

  ws.on("close", () => {
    clearTimeout(authTimeout);
    connections.delete(ws);
    if (state.stage === "authenticated") {
      log(`[ws] client ${state.client_id} disconnected`);
    }
  });

  ws.on("error", (err) => logErr("[ws] error", err));
});

// Sweep for clients that stopped heartbeating without a clean disconnect
// (e.g. the tunnel dropped silently, or the client's machine slept) —
// this is exactly the kind of case worth stress-testing once this is on
// a real, flaky home connection rather than localhost.
setInterval(() => {
  const now = Date.now();
  for (const [ws, state] of connections) {
    if (state.stage === "authenticated" && now - state.lastHeartbeat > HEARTBEAT_TIMEOUT_MS) {
      log(`[heartbeat] client ${state.client_id} timed out, closing`);
      ws.close(4008, "heartbeat timeout");
      connections.delete(ws);
    }
  }
}, HEARTBEAT_SWEEP_INTERVAL_MS);

httpServer.listen(PORT, () => {
  log(`delegation-server-scaffold listening on :${PORT}`);
  log(`  enroll:  POST http://localhost:${PORT}/enroll`);
  log(`  agent:   ws://localhost:${PORT}/agent`);
});
