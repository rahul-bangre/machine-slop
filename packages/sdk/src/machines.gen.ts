export type CreateMachine = { vcpus?: number | null, memory_mib?: number | null, };

export type MachineState = { "state": "running" } | { "state": "exited", message: string, };

export type MachineInfo = { id: string, state: MachineState, vcpus: number, memory_mib: number, boot_ms: number, };
