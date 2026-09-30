export interface EnrollRequest {
    pairing_code: string;
    public_key: string;
    hostname: string;
}
export interface EnrollResponse {
    client_id: string;
}
export interface EnrollError {
    error: string;
}
export type ClientMessage = {
    type: "hello";
    client_id: string;
} | {
    type: "challenge_response";
    signature: string;
} | {
    type: "heartbeat";
    timestamp: number;
    hostname: string;
};
export type ServerMessage = {
    type: "challenge";
    nonce: string;
} | {
    type: "auth_ok";
} | {
    type: "auth_failed";
    reason: string;
} | {
    type: "heartbeat_ack";
};
