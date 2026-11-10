export type CommandResult = {
    exit_code: number | null;
    stdout: string;
    stderr: string;
    timed_out: boolean;
};

export interface Sandbox {
    readonly reported_boot_ms: number | null;
    writeFile(relative_path: string, content: string): Promise<void>;
    run(script: string, timeout_ms: number): Promise<CommandResult>;
    close(): Promise<void>;
}

export interface SandboxProvider {
    readonly name: string;
    describe(): Record<string, unknown>;
    prepare(): Promise<void>;
    create(): Promise<Sandbox>;
    reclaim(): Promise<void>;
}

export class SandboxError extends Error {
    constructor(message: string, options?: ErrorOptions) {
        super(message, options);
        this.name = "SandboxError";
    }
}

export class ProviderNotReady extends Error {
    constructor(message: string) {
        super(message);
        this.name = "ProviderNotReady";
    }
}

export class OpenSandboxes {
    private readonly open = new Set<Sandbox>();

    track(sandbox: Sandbox): Sandbox {
        const tracked: Sandbox = {
            reported_boot_ms: sandbox.reported_boot_ms,
            writeFile: (path, content) => sandbox.writeFile(path, content),
            run: (script, timeout_ms) => sandbox.run(script, timeout_ms),
            close: async () => {
                this.open.delete(tracked);
                await sandbox.close();
            },
        };
        this.open.add(tracked);
        return tracked;
    }

    async closeAll(): Promise<void> {
        await Promise.allSettled(
            [...this.open].map((sandbox) => sandbox.close()),
        );
    }
}
