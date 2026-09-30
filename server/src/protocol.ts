// Wire protocol for the delegation-server <-> client-agent connection.
// Kept in one small file deliberately: this is the file worth sharing
// verbatim with the future React/TypeScript dashboard later, since the
// dashboard will want to speak the same enrollment/status shapes.

export interface EnrollRequest {
  pairing_code: string;
  public_key: string; // base64, 32-byte Ed25519 public key
  hostname: string;
}

export interface EnrollResponse {
  client_id: string;
}

export interface EnrollError {
  error: string;
}

// ---- WebSocket messages, client -> server ----

export type ClientMessage =
  | { type: "hello"; client_id: string }
  | { type: "challenge_response"; signature: string } // base64
  | { type: "heartbeat"; timestamp: number; hostname: string };

// ---- WebSocket messages, server -> client ----

export type ServerMessage =
  | { type: "challenge"; nonce: string } // base64, 16 random bytes
  | { type: "auth_ok" }
  | { type: "auth_failed"; reason: string }
  | { type: "heartbeat_ack" };
