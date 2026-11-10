import assert from "node:assert/strict";

import { Machine } from "../src/mod.ts";

export async function withMachine(
    run: (
        machine: Machine,
        terminate: () => void,
        url: string,
    ) => Promise<void>,
) {
    const daemon = new Deno.Command("./target/debug/agentd", {
        args: ["127.0.0.1:0"],
        stdout: "null",
        stderr: "piped",
    }).spawn();
    let drained: Promise<void> | undefined;
    const shutdown = { requested: false };
    const deadline = setTimeout(() => daemon.kill("SIGKILL"), 20_000);
    try {
        const url = await listenAddress(daemon.stderr);
        drained = daemon.stderr.pipeTo(new WritableStream());
        await run(new Machine(url), () => {
            shutdown.requested = true;
            daemon.kill("SIGTERM");
        }, url);
    } finally {
        if (!shutdown.requested) daemon.kill("SIGINT");
        const status = await daemon.status;
        if (drained) await drained;
        else await daemon.stderr.cancel();
        clearTimeout(deadline);
        assert.equal(status.code, 0, "daemon shuts down cleanly");
    }
}

async function listenAddress(
    stderr: ReadableStream<Uint8Array>,
): Promise<string> {
    const reader = stderr.getReader();
    const decoder = new TextDecoder();
    const chunks: string[] = [];
    try {
        while (true) {
            const { value, done } = await reader.read();
            assert.ok(!done, "daemon exited before reporting its address");
            chunks.push(decoder.decode(value, { stream: true }));
            const text = chunks.join("");
            if (!text.includes("\n")) continue;
            const url = text.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0];
            assert.ok(url, `unexpected daemon output: ${text}`);
            return url;
        }
    } finally {
        reader.releaseLock();
    }
}
