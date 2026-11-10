export type CreateMachine = { vcpus?: number | null, memory_mib?: number | null, idle_timeout_secs?: number | null, allowed_hosts?: Array<string>, };

export type UpdateMachine = { allowed_hosts: Array<string>, };

export type MachineState = { "state": "running" } | { "state": "exited", message: string, };

export type MachineInfo = { id: string, state: MachineState, vcpus: number, memory_mib: number, boot_ms: number, idle_timeout_secs: number, allowed_hosts: Array<string>, };
