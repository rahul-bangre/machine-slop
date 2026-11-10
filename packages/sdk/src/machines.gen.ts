export type CreateMachine = { vcpus?: number | null, memory_mib?: number | null, idle_timeout_secs?: number | null, allowed_hosts?: Array<string>, egress_credentials?: Array<EgressCredential>, egress_address?: string | null, snapshot_id?: string | null, };

export type EgressCredential = { host: string, headers: { [key in string]: string }, };

export type UpdateMachine = { allowed_hosts?: Array<string>, egress_credentials?: Array<EgressCredential>, };

export type MachineState = { "state": "running" } | { "state": "paused" } | { "state": "exited", message: string, };

export type MachineInfo = { id: string, state: MachineState, vcpus: number, memory_mib: number, boot_ms: number, idle_timeout_secs: number, allowed_hosts: Array<string>, credential_hosts: Array<string>, egress_address: string | null, };

export type CreateSnapshot = { machine_id: string, };

export type SnapshotInfo = { id: string, machine_id: string, vcpus: number, memory_mib: number, allowed_hosts: Array<string>, created_unix_ms: number, };

export type CreatePreview = { machine_id: string, port: number, ttl_secs?: number | null, };

export type PreviewInfo = { id: string, machine_id: string, port: number, url: string, expires_unix_ms: number, };
