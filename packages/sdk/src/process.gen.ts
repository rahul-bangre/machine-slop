export type StartProcess = { program: string, args?: Array<string> | null, env?: { [key in string]: string } | null, cwd?: string | null, stdin?: string | null, };

export type ProcessResult = { "kind": "exited", code: number | null, signal: number | null, output_complete: boolean, stdin_complete: boolean, } | { "kind": "unknown", message: string, };

export type ProcessStatus = { "state": "running" } | { "state": "finished", result: ProcessResult, };

export type ProcessInfo = { id: string, status: ProcessStatus, first_seq: number, next_seq: number, };

export type ProcessEvent = { "type": "stdout", seq: number, data: string, } | { "type": "stderr", seq: number, data: string, } | { "type": "finished", seq: number, result: ProcessResult, } | { "type": "gap", first_seq: number, };
