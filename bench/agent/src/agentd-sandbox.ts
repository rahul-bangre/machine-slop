import { Host, Machine, type Process } from "@machine-slop/sdk";

import {
    type CommandResult,
    OpenSandboxes,
    ProviderNotReady,
    type Sandbox,
    SandboxError,
    type SandboxProvider,
} from "./sandbox.ts";

const request_timeout_ms = 30_000;
const startup_timeout_ms = 20_000;
const machine_boot_timeout_ms = 60_000;
export const kill_grace_ms = 3_000;

type Lease = {
    machine: Machine;
    reported_boot_ms: number | null;
    release(work_directory: string | null): Promise<void>;
};

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

type ShellOptions = { cwd?: string; stdin?: string; arguments?: string[] };

async function shellWithin(
    timeout_ms: number,
    machine: Machine,
    script: string,
    options: ShellOptions = {},
): Promise<CommandResult> {
    const process = await machine.startProcess({
        program: "/bin/sh",
        args: ["-c", script, "sh", ...options.arguments ?? []],
        ...(options.cwd ? { cwd: options.cwd } : {}),
        ...(options.stdin === undefined ? {} : { stdin: options.stdin }),
    }, AbortSignal.timeout(request_timeout_ms));
    const finished = collect(process);
    finished.catch(() => {});
    let timed_out = false;
    let grace: ReturnType<typeof setTimeout> | undefined;
    const never_stopped = Promise.withResolvers<never>();
    const limit = setTimeout(() => {
        timed_out = true;
        process.kill(AbortSignal.timeout(kill_grace_ms)).catch(() => {});
        grace = setTimeout(
            () =>
                never_stopped.reject(
                    new SandboxError(
                        `command did not stop within ${kill_grace_ms}ms of being killed`,
                    ),
                ),
            kill_grace_ms,
        );
    }, timeout_ms);
    try {
        const result = await Promise.race([finished, never_stopped.promise]);
        return { ...result, timed_out };
    } finally {
        clearTimeout(limit);
        clearTimeout(grace);
    }
}

async function shell(
    machine: Machine,
    script: string,
    options: ShellOptions = {},
): Promise<CommandResult> {
    const result = await shellWithin(
        request_timeout_ms,
        machine,
        script,
        options,
    );
    if (result.timed_out) {
        throw new SandboxError(
            `sandbox setup command timed out: ${script}`,
        );
    }
    return result;
}

async function removeDirectory(
    machine: Machine,
    directory: string | null,
): Promise<void> {
    if (directory) {
        await shell(machine, 'rm -rf "$1"', { arguments: [directory] });
    }
}

class AgentdSandbox implements Sandbox {
    readonly reported_boot_ms: number | null;

    private constructor(
        private readonly lease: Lease,
        private readonly work_directory: string,
    ) {
        this.reported_boot_ms = lease.reported_boot_ms;
    }

    static async open(lease: Lease): Promise<AgentdSandbox> {
        try {
            const made = await shell(lease.machine, "mktemp -d");
            if (made.exit_code !== 0) {
                throw new SandboxError(
                    `cannot create work directory: ${made.stderr}`,
                );
            }
            return new AgentdSandbox(lease, made.stdout.trim());
        } catch (error) {
            await lease.release(null).catch(() => {});
            throw error;
        }
    }

    async writeFile(relative_path: string, content: string): Promise<void> {
        const written = await shell(
            this.lease.machine,
            'mkdir -p "$(dirname "$1")" && cat > "$1"',
            {
                cwd: this.work_directory,
                stdin: content,
                arguments: [relative_path],
            },
        );
        if (written.exit_code !== 0) {
            throw new SandboxError(
                `cannot write ${relative_path}: ${written.stderr}`,
            );
        }
    }

    run(script: string, timeout_ms: number): Promise<CommandResult> {
        return shellWithin(timeout_ms, this.lease.machine, script, {
            cwd: this.work_directory,
        });
    }

    close(): Promise<void> {
        return this.lease.release(this.work_directory);
    }
}

export async function startAgentd(
    binary: string,
): Promise<{ url: string; stop(): Promise<void> }> {
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
                    url,
                    async stop() {
                        try {
                            child.kill("SIGINT");
                        } catch (error) {
                            if (!(error instanceof TypeError)) throw error;
                        }
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

export function agentdProvider(
    target: { url: string } | { binary: string },
): SandboxProvider {
    const open = new OpenSandboxes();

    async function lease(): Promise<Lease> {
        if ("url" in target) {
            const machine = new Machine(target.url);
            return {
                machine,
                reported_boot_ms: null,
                release: (directory) => removeDirectory(machine, directory),
            };
        }
        const daemon = await startAgentd(target.binary);
        const machine = new Machine(daemon.url);
        return {
            machine,
            reported_boot_ms: null,
            async release(directory) {
                try {
                    await removeDirectory(machine, directory);
                } finally {
                    await daemon.stop();
                }
            },
        };
    }

    return {
        name: "agentd",
        describe: () => ({
            ...target,
            isolation: "none: commands run as the agentd user on its host",
            create_measures: "url" in target
                ? "a new work directory on an already running agentd"
                : "starting agentd and a new work directory",
        }),
        async prepare() {
            if ("url" in target) {
                await new Machine(target.url)
                    .health(AbortSignal.timeout(startup_timeout_ms))
                    .catch(() => {
                        throw new ProviderNotReady(
                            `agentd at ${target.url} is not reachable`,
                        );
                    });
                return;
            }
            const found = await Deno.stat(target.binary).catch(() => null);
            if (!found?.isFile) {
                throw new ProviderNotReady(
                    `${target.binary} does not exist; build agentd first`,
                );
            }
        },
        create: async () => open.track(await AgentdSandbox.open(await lease())),
        reclaim: () => open.closeAll(),
    };
}

export function machinedProvider(url: string): SandboxProvider {
    const host = new Host(url);
    const open = new OpenSandboxes();

    async function destroyEveryMachine(): Promise<void> {
        const machines = await host.machines(
            AbortSignal.timeout(request_timeout_ms),
        );
        await Promise.all(
            machines.map((machine) =>
                host.machine(machine.id).destroy(
                    AbortSignal.timeout(request_timeout_ms),
                )
            ),
        );
    }

    async function lease(): Promise<Lease> {
        await destroyEveryMachine();
        try {
            const { machine, info } = await host.createMachine(
                {},
                AbortSignal.timeout(machine_boot_timeout_ms),
            );
            return {
                machine,
                reported_boot_ms: info.boot_ms,
                release: () =>
                    machine.destroy(AbortSignal.timeout(request_timeout_ms)),
            };
        } catch (error) {
            await destroyEveryMachine().catch(() => {});
            throw error;
        }
    }

    return {
        name: "machined",
        describe: () => ({
            url,
            isolation: "one Firecracker microVM per attempt, no network",
            create_measures:
                "cold boot of a new microVM until agentd is healthy, and a new work directory",
            host_ownership:
                "exclusive: every machine found on the host is destroyed before each create and when the run ends",
        }),
        async prepare() {
            const machines = await host.machines(
                AbortSignal.timeout(startup_timeout_ms),
            ).catch(() => {
                throw new ProviderNotReady(
                    `machined at ${url} is not reachable; run make vm-start`,
                );
            });
            if (machines.length > 0) {
                throw new ProviderNotReady(
                    `machined at ${url} already has ${machines.length} running. The bench needs the host to itself and destroys every machine it finds. Restart machined to clear them.`,
                );
            }
        },
        create: async () => open.track(await AgentdSandbox.open(await lease())),
        async reclaim() {
            await open.closeAll();
            await destroyEveryMachine();
        },
    };
}
