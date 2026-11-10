export type CommandResult = {
    exit_code: number | null;
    stdout: string;
    stderr: string;
    timed_out: boolean;
};

export interface Sandbox {
    writeFile(relative_path: string, content: string): Promise<void>;
    run(script: string, timeout_ms: number): Promise<CommandResult>;
    close(): Promise<void>;
}

export interface SandboxProvider {
    readonly name: string;
    describe(): Record<string, unknown>;
    create(): Promise<Sandbox>;
}

export class SandboxError extends Error {
    constructor(message: string, options?: ErrorOptions) {
        super(message, options);
        this.name = "SandboxError";
    }
}
