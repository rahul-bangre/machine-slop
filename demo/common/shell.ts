import type { Machine } from "../../packages/sdk/src/mod.ts";

export type ShellResult = {
    exit_code: number | null;
    output: string;
    timed_out: boolean;
    ms: number;
};

const decoder = new TextDecoder();

function decode(data: string): string {
    return decoder.decode(Uint8Array.from(atob(data), (c) => c.charCodeAt(0)));
}

export async function shell(
    machine: Machine,
    script: string,
    timeout_ms = 60_000,
    cwd = "/workspace/app",
): Promise<ShellResult> {
    const started_at = performance.now();
    const process = await machine.startProcess({
        program: "/bin/bash",
        args: ["-lc", script],
        cwd,
    });
    const deadline = AbortSignal.timeout(timeout_ms);
    let output = "";
    let exit_code: number | null = null;
    let timed_out = false;
    try {
        for await (const event of process.events(0, deadline)) {
            if (event.type === "stdout" || event.type === "stderr") {
                output += decode(event.data);
            } else if (event.type === "finished") {
                exit_code = event.result.kind === "exited"
                    ? event.result.code
                    : null;
            }
        }
    } catch (error) {
        if (!deadline.aborted) throw error;
        timed_out = true;
        await process.kill().catch(() => {});
    }
    return {
        exit_code,
        output,
        timed_out,
        ms: Math.round(performance.now() - started_at),
    };
}

export function tail(text: string, lines: number, width = 160): string[] {
    return text.split("\n").map((line) => line.trimEnd()).filter(Boolean)
        .slice(-lines).map((line) =>
            line.length > width ? `${line.slice(0, width - 1)}…` : line
        );
}
