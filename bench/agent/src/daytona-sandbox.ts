import { type CreateSandboxFromImageParams, Daytona } from "@daytona/sdk";
import {
    type CommandResult,
    ProviderNotReady,
    SandboxError,
    type SandboxProvider,
} from "./sandbox.ts";

const request_ms = 30_000;
const create_ms = 120_000;
const directory = "/tmp/agent-task";
const image = "python:3.12-slim";

// A structural boundary keeps the lifecycle tests independent of cloud access.
export type DaytonaRemote = {
    process: {
        executeCommand(
            command: string,
            cwd?: string,
            env?: Record<string, string>,
            timeout?: number,
        ): Promise<{ exitCode: number; result: string }>;
    };
};
export type DaytonaClient = {
    create(
        params: CreateSandboxFromImageParams,
        options: { timeout: number },
    ): Promise<DaytonaRemote>;
    get(name: string): Promise<DaytonaRemote>;
    delete(
        remote: DaytonaRemote,
        timeout: number,
        wait: boolean,
    ): Promise<void>;
};

async function within<T>(promise: Promise<T>, ms: number): Promise<T> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
        return await Promise.race([
            promise,
            new Promise<never>((_, reject) => {
                timer = setTimeout(
                    () =>
                        reject(
                            new SandboxError("Daytona local deadline exceeded"),
                        ),
                    ms,
                );
            }),
        ]);
    } finally {
        clearTimeout(timer);
    }
}

export function daytonaProvider(client?: DaytonaClient): SandboxProvider {
    const owned = new Map<string, DaytonaRemote | null>();
    const deleting = new Map<string, Promise<void>>();
    let closing = false;
    const api = (): DaytonaClient =>
        client ??= new Daytona({
            requestTimeoutMs: request_ms,
            otelEnabled: false,
        });
    function remove(name: string): Promise<void> {
        const pending = deleting.get(name);
        if (pending) return pending;
        const deletion = destroy(name).finally(() => deleting.delete(name));
        deleting.set(name, deletion);
        return deletion;
    }
    async function destroy(name: string): Promise<void> {
        if (!owned.has(name)) return;
        const remote = owned.get(name) ??
            await within(api().get(name), request_ms);
        await within(api().delete(remote, 30, true), request_ms + 5_000);
        owned.delete(name);
    }
    return {
        name: "daytona",
        describe: () => ({
            image,
            cpu: 2,
            memory_gib: 4,
            isolation: "Daytona cloud sandbox",
            create_measures:
                "image sandbox creation until ready, plus work directory setup",
        }),
        prepare() {
            if (!client && !Deno.env.get("DAYTONA_API_KEY")) {
                throw new ProviderNotReady(
                    "the daytona sandbox needs DAYTONA_API_KEY",
                );
            }
            api();
            return Promise.resolve();
        },
        async reclaim() {
            closing = true;
            const results = await Promise.allSettled(
                [...owned.keys()].map(remove),
            );
            const errors = results.filter((r) => r.status === "rejected").map((
                r,
            ) => r.reason);
            if (errors.length) {
                throw new AggregateError(errors, "Daytona cleanup incomplete");
            }
        },
        async create() {
            if (closing) throw new SandboxError("Daytona provider is closed");
            const name = `agent-bench-${crypto.randomUUID()}`;
            owned.set(name, null);
            let usable = true;
            let busy = false;
            const creation = api().create({
                name,
                image,
                resources: { cpu: 2, memory: 4 },
                ttlMinutes: 30,
                autoStopInterval: 0,
                autoDeleteInterval: 0,
            }, { timeout: create_ms / 1000 });
            // The local deadline cannot cancel creation. Clean up a late response too.
            creation.then(async (remote) => {
                if (!usable || closing) {
                    owned.set(name, remote);
                    await remove(name);
                }
            }).catch(() => {});
            try {
                const remote = await within(creation, create_ms + 5_000);
                owned.set(name, remote);
                if (closing) {
                    throw new SandboxError("Daytona provider is closed");
                }
                const execute = async (
                    script: string,
                    timeout_ms: number,
                ): Promise<CommandResult> => {
                    if (!usable || busy || closing) {
                        throw new SandboxError(
                            "Daytona sandbox is closed or busy",
                        );
                    }
                    if (
                        !Number.isSafeInteger(timeout_ms) || timeout_ms < 1 ||
                        timeout_ms > 2_147_483_647
                    ) {
                        throw new RangeError(
                            "timeout must be a positive integer",
                        );
                    }
                    busy = true;
                    let expired = false;
                    let timer: ReturnType<typeof setTimeout> | undefined;
                    try {
                        const result = await Promise.race([
                            remote.process.executeCommand(
                                script,
                                directory,
                                undefined,
                                Math.ceil(timeout_ms / 1000) + 1,
                            ),
                            new Promise<never>((_, reject) => {
                                timer = setTimeout(() => {
                                    expired = true;
                                    reject(
                                        new SandboxError(
                                            "command deadline exceeded",
                                        ),
                                    );
                                }, timeout_ms);
                            }),
                        ]);
                        return {
                            exit_code: result.exitCode,
                            stdout: result.result,
                            stderr: "",
                            timed_out: false,
                        };
                    } catch (error) {
                        clearTimeout(timer);
                        usable = false;
                        await remove(name); // Only report timeout after confirmed destruction.
                        if (expired) {
                            return {
                                exit_code: null,
                                stdout: "",
                                stderr: "",
                                timed_out: true,
                            };
                        }
                        throw error;
                    } finally {
                        clearTimeout(timer);
                        busy = false;
                    }
                };
                const setup = await within(
                    remote.process.executeCommand(
                        `mkdir -p ${directory}`,
                        undefined,
                        undefined,
                        30,
                    ),
                    request_ms,
                );
                if (setup.exitCode !== 0) {
                    throw new SandboxError(
                        `Daytona setup failed: ${setup.result}`,
                    );
                }
                return {
                    reported_boot_ms: null,
                    run: execute,
                    async writeFile(path, content) {
                        if (
                            !path || path.startsWith("/") ||
                            path.split("/").some((part) =>
                                part === ".." || part === ""
                            )
                        ) throw new SandboxError("invalid relative path");
                        const quote = (s: string) =>
                            "'" + s.replaceAll("'", "'\\''") + "'";
                        const result = await execute(
                            `mkdir -p -- "$(dirname -- ${
                                quote(path)
                            })" && printf %s ${quote(content)} > ${
                                quote(path)
                            }`,
                            request_ms,
                        );
                        if (result.timed_out || result.exit_code !== 0) {
                            throw new SandboxError("Daytona file write failed");
                        }
                    },
                    async close() {
                        usable = false;
                        await remove(name);
                    },
                };
            } catch (error) {
                usable = false;
                try {
                    await remove(name);
                } catch (cleanup) {
                    throw new AggregateError(
                        [error, cleanup],
                        `Daytona create failed; cleanup pending for ${name}`,
                    );
                }
                throw error;
            }
        },
    };
}
