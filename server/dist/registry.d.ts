export interface ClientRecord {
    client_id: string;
    public_key: string;
    hostname: string;
    enrolled_at: string;
}
export declare function createPairingCode(ttlMs?: number): Promise<string>;
export declare function consumePairingCode(code: string): Promise<boolean>;
export declare function addClient(record: ClientRecord): Promise<void>;
export declare function getClient(clientId: string): Promise<ClientRecord | undefined>;
