export type CreateMachine = { vcpus?: number | null, memory_mib?: number | null, idle_timeout_secs?: number | null, allowed_hosts?: Array<string>, snapshot_id?: string | null, };

export type UpdateMachine = { allowed_hosts: Array<string>, };

export type MachineState = { "state": "running" } | { "state": "paused" } | { "state": "exited", message: string, };

export type MachineInfo = { id: string, state: MachineState, vcpus: number, memory_mib: number, boot_ms: number, idle_timeout_secs: number, allowed_hosts: Array<string>, };

export type CreateSnapshot = { machine_id: string, };

export type SnapshotInfo = { id: string, machine_id: string, vcpus: number, memory_mib: number, allowed_hosts: Array<string>, created_unix_ms: number, stored_bytes: number, };

export type Quotas = { running_machines: number, memory_mib: number, disk_gib: number, creates_per_second: number, };

export type TenantUsage = { tenant: string, quotas: Quotas, vcpu_ms: number, memory_mib_ms: number, snapshot_mib_ms: number, as_of_unix_ms: number, };
