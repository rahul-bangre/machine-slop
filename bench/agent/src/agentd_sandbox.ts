import { Host, Machine, type Process } from "@machine-slop/sdk";

import {
    type CommandResult,
    type Sandbox,
    SandboxError,
    type SandboxProvider,
} from "./sandbox.ts";

const setup_timeout_ms = 30_000;
const startup_timeout_ms = 20_000;
const machine_boot_timeout_ms = 60_000;

type Daemon = {
    machine: Machine;
    keeps_files_after_stop: boolean;
    stop(): Promise<void>;
};

async function connect(url: string): Promise<Daemon> {
    const machine = new Machine(url);
    await machine.health(AbortSignal.timeout(startup_timeout_ms));
    return {
        machine,
        keeps_files_after_stop: true,
        stop: () => Promise.resolve(),
    };
}

async function boot(host: Host): Promise<Daemon> {
    const { machine } = await host.createMachine(
        {},
        AbortSignal.timeout(machine_boot_timeout_ms),
    );
    return {
        machine,
        keeps_files_after_stop: false,
        stop: () => machine.destroy(),
    };
}

async function spawn(binary: string): Promise<Daemon> {
    const child = new Deno.Command(binary, {
        args: ["127.0.0.1:0"],
        stdin: "null",
        stdout: "null",
        stderr: "piped",
    }).spawn();
    const decoder = new TextDecoder();
    let announced = "";
    const reader = child.stderr.getReader();
    const deadline = setTimeout(
        () => child.kill("SIGKILL"),
        startup_timeout_ms,
    );
    try {
        while (true) {
            const url = announced.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0];
            if (url) {
                reader.releaseLock();
                const drained = child.stderr.pipeTo(new WritableStream());
                return {
                    machine: new Machine(url),
                    keeps_files_after_stop: true,
                    async stop() {
                        child.kill("SIGINT");
                        await child.status;
                        await drained.catch(() => {});
                    },
                };
            }
            const { value, done } = await reader.read();
            if (done) {
                throw new SandboxError(
                    `agentd exited before listening: ${announced}`,
                );
            }
            announced += decoder.decode(value, { stream: true });
        }
    } finally {
        clearTimeout(deadline);
    }
}

function decodeBase64(data: string): Uint8Array {
    return Uint8Array.from(atob(data), (char) => char.charCodeAt(0));
}

async function collect(process: Process): Promise<CommandResult> {
    const streams = { stdout: [] as Uint8Array[], stderr: [] as Uint8Array[] };
    for await (const event of process.events()) {
        if (event.type === "stdout" || event.type === "stderr") {
            streams[event.type].push(decodeBase64(event.data));
        }
    }
    const result = await process.wait();
    if (result.kind !== "exited") {
        throw new SandboxError(`agentd lost the process: ${result.message}`);
    }
    const text = (chunks: Uint8Array[]) =>
        new TextDecoder().decode(
            Uint8Array.from(chunks.flatMap((chunk) => [...chunk])),
        );
    return {
        exit_code: result.code,
        stdout: text(streams.stdout),
        stderr: text(streams.stderr),
        timed_out: false,
    };
}

class AgentdSandbox implements Sandbox {
    constructor(
        private readonly daemon: Daemon,
        private readonly work_directory: string,
    ) {}

    static async open(daemon: Daemon): Promise<AgentdSandbox> {
        const made = await collect(
            await daemon.machine.startProcess({
                program: "/bin/sh",
                args: ["-c", "mktemp -d"],
            }, AbortSignal.timeout(setup_timeout_ms)),
        );
        if (made.exit_code !== 0) {
            throw new SandboxError(
                `cannot create work directory: ${made.stderr}`,
            );
        }
        return new AgentdSandbox(daemon, made.stdout.trim());
    }

    async writeFile(relative_path: string, content: string): Promise<void> {
        const written = await collect(
            await this.daemon.machine.startProcess({
                program: "/bin/sh",
                args: [
                    "-c",
                    'mkdir -p "$(dirname "$1")" && cat > "$1"',
                    "write",
                    relative_path,
                ],
                cwd: this.work_directory,
                stdin: content,
            }, AbortSignal.timeout(setup_timeout_ms)),
        );
        if (written.exit_code !== 0) {
            throw new SandboxError(
                `cannot write ${relative_path}: ${written.stderr}`,
            );
        }
    }

    async run(script: string, timeout_ms: number): Promise<CommandResult> {
        const process = await this.daemon.machine.startProcess({
            program: "/bin/sh",
            args: ["-c", script],
            cwd: this.work_directory,
        });
        let timed_out = false;
        const timer = setTimeout(() => {
            timed_out = true;
            process.kill().catch(() => {});
        }, timeout_ms);
        try {
            return { ...await collect(process), timed_out };
        } finally {
            clearTimeout(timer);
        }
    }

    async close(): Promise<void> {
        try {
            if (this.daemon.keeps_files_after_stop) {
                await this.run(
                    `rm -rf "${this.work_directory}"`,
                    setup_timeout_ms,
                );
            }
        } finally {
            await this.daemon.stop();
        }
    }
}

async function open(daemon: Daemon): Promise<Sandbox> {
    try {
        return await AgentdSandbox.open(daemon);
    } catch (error) {
        await daemon.stop();
        throw error;
    }
}

export function machinedProvider(url: string): SandboxProvider {
    const host = new Host(url);
    return {
        name: "machined",
        describe: () => ({
            url,
            isolation: "one Firecracker microVM per attempt, no network",
            create_measures:
                "cold boot of a new microVM until agentd is healthy, and a new work directory",
        }),
        create: async () => open(await boot(host)),
    };
}

export function agentdProvider(
    target: { url: string } | { binary: string },
): SandboxProvider {
    return {
        name: "agentd",
        describe: () => ({
            ...target,
            isolation: "none: commands run as the agentd user on its host",
            create_measures: "url" in target
                ? "a new work directory on an already running agentd"
                : "starting agentd and a new work directory",
        }),
        create: async () =>
            open(
                "url" in target
                    ? await connect(target.url)
                    : await spawn(target.binary),
            ),
    };
}
