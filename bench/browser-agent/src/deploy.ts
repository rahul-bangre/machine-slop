import type { CommandResult, Sandbox } from "../../agent/src/sandbox.ts";
import type { RuntimeConfig } from "../runtime/worker.ts";

const root = new URL("../../../", import.meta.url);
export const deno_version = "2.9.7";

export function quote(text: string): string {
    return "'" + text.replaceAll("'", "'\\''") + "'";
}

async function filesIn(path: string): Promise<[string, string][]> {
    const result: [string, string][] = [];
    for await (const entry of Deno.readDir(new URL(path, root))) {
        const file = `${path}/${entry.name}`;
        if (entry.isDirectory) result.push(...await filesIn(file));
        else if (entry.isFile) {
            result.push([file, await Deno.readTextFile(new URL(file, root))]);
        }
    }
    return result;
}

export async function payload(): Promise<
    { files: [string, string][]; sha256: string }
> {
    const files: [string, string][] = [
        ...await filesIn("bench/browser-agent/runtime"),
        ...await filesIn("bench/browser/src"),
        [
            "bench/browser/chromium.json",
            await Deno.readTextFile(
                new URL("bench/browser/chromium.json", root),
            ),
        ],
        [
            "imports.json",
            await Deno.readTextFile(
                new URL("bench/browser-agent/deno.json", root),
            ),
        ],
        ["runtime.lock", await Deno.readTextFile(new URL("deno.lock", root))],
    ];
    files.sort(([left], [right]) => left.localeCompare(right));
    const hash = await crypto.subtle.digest(
        "SHA-256",
        new TextEncoder().encode(JSON.stringify(files)),
    );
    return {
        files,
        sha256: [...new Uint8Array(hash)].map((byte) =>
            byte.toString(16).padStart(2, "0")
        ).join(""),
    };
}

export class RemoteRuntime {
    private static readonly open = new Set<RemoteRuntime>();
    private static stopping = false;
    private launch: Promise<CommandResult> | null = null;
    private cleanup: Promise<void> | null = null;

    private constructor(
        private readonly sandbox: Sandbox,
        private readonly deno: string,
    ) {
        RemoteRuntime.open.add(this);
    }

    static async reclaim(): Promise<void> {
        RemoteRuntime.stopping = true;
        const results = await Promise.allSettled(
            [...RemoteRuntime.open].map((runtime) => runtime.close()),
        );
        const failures = results.flatMap((result) =>
            result.status === "rejected" ? [result.reason] : []
        );
        if (failures.length) {
            throw new AggregateError(failures, "runtime cleanup incomplete");
        }
    }

    private command(
        operation: string,
        input = "{}",
        timeout_ms = 40_000,
    ): Promise<CommandResult> {
        return this.sandbox.run(
            `${
                quote(this.deno)
            } run --no-config -A bench/browser-agent/runtime/client.ts control.json ${operation} ${
                quote(input)
            }`,
            timeout_ms,
        );
    }

    async run(input: string, timeout_ms: number): Promise<CommandResult> {
        return await this.command("command", input, timeout_ms);
    }

    async report(): Promise<{ checker_failure: string | null }> {
        const result = await this.command("report");
        requireSuccess(result, "runtime report");
        const report: unknown = JSON.parse(result.stdout);
        if (
            !report || typeof report !== "object" ||
            !("checker_failure" in report) ||
            (report.checker_failure !== null &&
                typeof report.checker_failure !== "string")
        ) {
            throw new Error("runtime report has no valid checker outcome");
        }
        return { ...report, checker_failure: report.checker_failure };
    }

    close(): Promise<void> {
        return this.cleanup ??= this.stop().then(() => {
            RemoteRuntime.open.delete(this);
        }).catch((error) => {
            this.cleanup = null;
            throw error;
        });
    }

    private async stop(): Promise<void> {
        await this.launch?.catch(() => {});
        const stopped = await this.command("stop", "{}", 10_000).catch(() =>
            null
        );
        if (stopped?.exit_code === 0 && !stopped.timed_out) {
            const exited = await this.sandbox.run(
                "i=0; while test -f control.json && test $i -lt 100; do sleep 0.1; i=$((i+1)); done; test ! -f control.json",
                15_000,
            );
            if (exited.exit_code === 0 && !exited.timed_out) return;
        }
        const killed = await this.sandbox.run(
            `${
                quote(this.deno)
            } run --no-config -A bench/browser-agent/runtime/cleanup.ts`,
            10_000,
        );
        requireSuccess(killed, "runtime cleanup");
    }

    static async start(
        sandbox: Sandbox,
        files: [string, string][],
        config: RuntimeConfig,
        deno: string,
    ): Promise<RemoteRuntime> {
        if (RemoteRuntime.stopping) throw new Error("runtime admission closed");
        for (const [path, content] of files) {
            await sandbox.writeFile(path, content);
        }
        await sandbox.writeFile("runtime-config.json", JSON.stringify(config));
        const version = await sandbox.run(`${quote(deno)} --version`, 10_000);
        requireSuccess(version, "Deno preflight");
        if (!version.stdout.startsWith(`deno ${deno_version} `)) {
            throw new Error(
                `expected Deno ${deno_version}, got ${version.stdout.trim()}`,
            );
        }
        const cached = await sandbox.run(
            `${
                quote(deno)
            } cache --no-config --import-map=imports.json --lock=runtime.lock --frozen bench/browser-agent/runtime/worker.ts`,
            120_000,
        );
        requireSuccess(cached, "runtime dependency preflight");
        if (RemoteRuntime.stopping) throw new Error("runtime admission closed");
        const runtime = new RemoteRuntime(sandbox, deno);
        try {
            runtime.launch = sandbox.run(
                `${
                    quote(deno)
                } run --no-config -A bench/browser-agent/runtime/launch.ts`,
                15_000,
            );
            requireSuccess(await runtime.launch, "runtime launch");
            const ready = await sandbox.run(
                "i=0; while test ! -f control.json && test $i -lt 200; do sleep 0.1; i=$((i+1)); done; if test -f control.json; then exit 0; else cat runtime.log; exit 1; fi",
                25_000,
            );
            requireSuccess(ready, "browser startup");
            if (RemoteRuntime.stopping) {
                throw new Error("runtime admission closed");
            }
            return runtime;
        } catch (error) {
            await runtime.close().catch(() => {});
            throw error;
        }
    }
}

function requireSuccess(result: CommandResult, label: string): void {
    if (result.timed_out || result.exit_code !== 0) {
        throw new Error(
            `${label}: ${
                result.timed_out ? "timed out" : result.stdout + result.stderr
            }`,
        );
    }
}
