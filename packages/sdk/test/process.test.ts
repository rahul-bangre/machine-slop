import assert from "node:assert/strict";

import { HttpError, Machine } from "../src/mod.ts";
import type { OutputEvent, Process } from "../src/mod.ts";

async function withMachine(
    run: (machine: Machine, terminate: () => void) => Promise<void>,
) {
    const daemon = new Deno.Command("./target/debug/agentd", {
        args: ["127.0.0.1:0"],
        stdout: "null",
        stderr: "piped",
    }).spawn();
    const shutdown = { requested: false };
    const deadline = setTimeout(() => daemon.kill("SIGKILL"), 20_000);
    try {
        const url = await listenAddress(daemon.stderr);
        await run(new Machine(url), () => {
            shutdown.requested = true;
            daemon.kill("SIGTERM");
        });
    } finally {
        if (!shutdown.requested) daemon.kill("SIGINT");
        const status = await daemon.status;
        await daemon.stderr.cancel();
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

function decode(
    events: OutputEvent[],
    stream: "stdout" | "stderr",
): Uint8Array {
    return Uint8Array.from(
        events.filter((event) => event.type === stream).flatMap((event) => {
            assert.ok(event.type === "stdout" || event.type === "stderr");
            return [...atob(event.data)].map((char) => char.charCodeAt(0));
        }),
    );
}

function collect(process: Process, cursor = 0): Promise<OutputEvent[]> {
    return Array.fromAsync(process.events(cursor));
}

Deno.test("command arguments, environment, cwd, stdin, binary output, and exit status", () =>
    withMachine(async (machine) => {
        const dir = await Deno.makeTempDir();
        const input = "input".repeat(4096);
        try {
            const process = await machine.startProcess({
                program: "/bin/sh",
                args: [
                    "-c",
                    'printf "%s\\n" "$VALUE" "$1"; cat; printf "\\000\\377"; pwd >&2; exit 7',
                    "command",
                    "literal; argument",
                ],
                env: { VALUE: "configured" },
                cwd: dir,
                stdin: input,
            });
            const events = await collect(process);
            assert.deepEqual(
                decode(events, "stdout"),
                new Uint8Array([
                    ...new TextEncoder().encode(
                        `configured\nliteral; argument\n${input}`,
                    ),
                    0,
                    255,
                ]),
            );
            const cwd = new TextDecoder().decode(decode(events, "stderr"))
                .trim();
            assert.equal(await Deno.realPath(cwd), await Deno.realPath(dir));
            const result = {
                kind: "exited",
                code: 7,
                signal: null,
                output_complete: true,
                stdin_complete: true,
            };
            assert.deepEqual(await process.wait(), result);
            assert.deepEqual((await process.info()).status, {
                state: "finished",
                result,
            });
            assert.equal(events.at(-1)?.type, "finished");
        } finally {
            await Deno.remove(dir);
        }
    }));

Deno.test("disconnecting output and wait does not cancel execution; cursors replay once", () =>
    withMachine(async (machine) => {
        const dir = await Deno.makeTempDir();
        try {
            const process = await machine.startProcess({
                program: "/bin/sh",
                args: [
                    "-c",
                    'printf first; while [ ! -f "$1/release" ]; do sleep 0.01; done; printf second',
                    "command",
                    dir,
                ],
            });
            const stream = process.events();
            const first = await stream.next();
            assert.ok(!first.done && first.value.type === "stdout");
            await stream.return(undefined);
            const controller = new AbortController();
            const waiting = process.wait(controller.signal);
            controller.abort();
            await assert.rejects(waiting, { name: "AbortError" });
            await Deno.writeTextFile(`${dir}/release`, "");
            assert.equal((await process.wait()).kind, "exited");
            const reconnected = machine.process(process.id);
            const rest = await collect(reconnected, first.value.seq + 1);
            assert.equal(
                new TextDecoder().decode(decode(rest, "stdout")),
                "second",
            );
            assert.equal(rest.at(-1)?.type, "finished");
            assert.equal((await collect(reconnected)).length, rest.length + 1);
            assert.deepEqual(
                await collect(reconnected, (await reconnected.info()).next_seq),
                [],
            );
        } finally {
            await Deno.remove(dir, { recursive: true });
        }
    }));

Deno.test("immediate and repeated kill requests terminate the direct child", () =>
    withMachine(async (machine) => {
        const process = await machine.startProcess({
            program: "/bin/sleep",
            args: ["30"],
        });
        await Promise.all([process.kill(), process.kill()]);
        const result = await process.wait();
        assert.equal(result.kind, "exited");
        if (result.kind !== "exited") throw new Error("expected observed exit");
        assert.equal(result.signal, 9);
        assert.equal(result.code, null);
        await process.kill();
        assert.deepEqual(await process.wait(), result);
    }));

Deno.test("bounded replay reports expired cursors without losing completion", () =>
    withMachine(async (machine) => {
        const process = await machine.startProcess({
            program: "/bin/sh",
            args: ["-c", "head -c 2097152 /dev/zero"],
        });
        const result = await process.wait();
        assert.deepEqual(result, {
            kind: "exited",
            code: 0,
            signal: null,
            output_complete: true,
            stdin_complete: true,
        });
        const info = await process.info();
        assert.ok(info.first_seq > 0);
        await assert.rejects(
            collect(process),
            (error) => error instanceof HttpError && error.status === 410,
        );
        const retained = await collect(process, info.first_seq);
        assert.equal(retained.at(-1)?.type, "finished");
        assert.ok(decode(retained, "stdout").length <= 1024 * 1024);
        await assert.rejects(
            collect(process, info.next_seq + 1),
            (error) => error instanceof HttpError && error.status === 400,
        );
    }));

Deno.test("invalid starts, oversized bodies, and unknown process IDs are explicit HTTP failures", () =>
    withMachine(async (machine) => {
        const cases = [
            { command: { program: "/missing/program" }, status: 422 },
            { command: { program: "/bin/true", typo: true }, status: 422 },
            {
                command: { program: "/bin/true", stdin: "x".repeat(65536) },
                status: 413,
            },
        ];
        for (const test_case of cases) {
            await assert.rejects(
                machine.startProcess(test_case.command),
                (error) =>
                    error instanceof HttpError &&
                    error.status === test_case.status,
            );
        }
        await assert.rejects(
            machine.process("missing").info(),
            (error) => error instanceof HttpError && error.status === 404,
        );
    }));

Deno.test("event transport rejects lost completion, broken framing, gaps, and skipped sequences", async () => {
    const stdout = '{"type":"stdout","seq":0,"data":"aGk="}\n';
    const cases = [
        { body: stdout, error: /ended before completion/ },
        {
            body: stdout + '{"type":"finished",',
            error: /ended before completion/,
        },
        { body: '{"type":"gap","first_seq":10}\n', error: /output expired/ },
        {
            body: '{"type":"stdout","seq":1,"data":"aGk="}\n',
            error: /Invalid process event sequence/,
        },
    ];
    for (const test_case of cases) {
        const server = Deno.serve({
            hostname: "127.0.0.1",
            port: 0,
            onListen() {},
        }, () => new Response(test_case.body));
        try {
            const process = new Machine(`http://127.0.0.1:${server.addr.port}`)
                .process("example");
            await assert.rejects(collect(process), test_case.error);
        } finally {
            await server.shutdown();
        }
    }
});

Deno.test("SIGTERM reaps the active child and delivers its final event before shutdown", () =>
    withMachine(async (machine, terminate) => {
        const process = await machine.startProcess({
            program: "/bin/sh",
            args: ["-c", "printf ready; exec sleep 30"],
        });
        const stream = process.events();
        try {
            const first = await stream.next();
            assert.ok(!first.done && first.value.type === "stdout");
            terminate();
            const rest = await Array.fromAsync(stream);
            const final = rest.at(-1);
            assert.ok(final?.type === "finished");
            assert.equal(final.result.kind, "exited");
            if (final.result.kind === "exited") {
                assert.equal(final.result.signal, 9);
            }
        } finally {
            await stream.return(undefined);
        }
    }));
