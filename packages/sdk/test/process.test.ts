import assert from "node:assert/strict";

import { HttpError, Machine } from "../src/mod.ts";
import type { OutputEvent, Process } from "../src/mod.ts";
import { withMachine } from "./agentd.ts";

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

Deno.test("interactive stdin streams in order and retried writes are not repeated", () =>
    withMachine(async (machine) => {
        const process = await machine.startProcess({
            program: "/bin/cat",
            keep_stdin_open: true,
        });
        const stream = process.events();
        try {
            let offset = await process.writeStdin(0, "hello ");
            const echoed = await stream.next();
            assert.ok(!echoed.done && echoed.value.type === "stdout");
            assert.equal(atob(echoed.value.data), "hello ");
            assert.equal(await process.writeStdin(0, "hello "), offset);
            await assert.rejects(
                process.writeStdin(offset + 1, "x"),
                (error) => error instanceof HttpError && error.status === 409,
            );
            offset = await process.writeStdin(offset, new Uint8Array([0, 255]));
            assert.deepEqual(
                {
                    stdin_offset: (await process.info()).stdin_offset,
                    stdin_open: (await process.info()).stdin_open,
                },
                { stdin_offset: 8, stdin_open: true },
            );
            await process.closeStdin(offset);
            const rest = [echoed.value, ...await Array.fromAsync(stream)];
            assert.deepEqual(
                decode(rest, "stdout"),
                new Uint8Array([
                    ...new TextEncoder().encode("hello "),
                    0,
                    255,
                ]),
            );
            assert.deepEqual(await process.wait(), {
                kind: "exited",
                code: 0,
                signal: null,
                output_complete: true,
                stdin_complete: true,
            });
            await assert.rejects(
                process.writeStdin(offset, "late"),
                (error) => error instanceof HttpError && error.status === 409,
            );
        } finally {
            await stream.return(undefined);
        }
    }));

Deno.test("a terminal process reports and applies its size", () =>
    withMachine(async (machine) => {
        const process = await machine.startProcess({
            program: "/bin/sh",
            args: ["-c", "stty size; read _; stty size"],
            terminal: { rows: 24, cols: 80 },
        });
        await process.resize({ rows: 50, cols: 132 });
        assert.deepEqual((await process.info()).terminal, {
            rows: 50,
            cols: 132,
        });
        await process.writeStdin(0, "\n");
        await assert.rejects(
            process.closeStdin(1),
            (error) => error instanceof HttpError && error.status === 409,
        );
        const output = new TextDecoder().decode(
            decode(await collect(process), "stdout"),
        );
        assert.match(output, /50 132\r\n$/);
        assert.equal((await process.wait()).kind, "exited");
        await assert.rejects(
            process.resize({ rows: 10, cols: 10 }),
            (error) => error instanceof HttpError && error.status === 409,
        );
    }));

Deno.test("immediate and repeated kill requests terminate the process group", () =>
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
            {
                command: {
                    program: "/bin/true",
                    terminal: { rows: 0, cols: 80 },
                },
                status: 422,
            },
            {
                command: {
                    program: "/bin/true",
                    terminal: { rows: 24, cols: 80 },
                    keep_stdin_open: false,
                },
                status: 422,
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

Deno.test("process metrics distinguish running children from retained replay records", () =>
    withMachine(async (machine, _terminate, url) => {
        const metrics = async () => {
            const response = await fetch(`${url}/metrics`);
            assert.equal(response.status, 200);
            assert.match(
                response.headers.get("content-type") ?? "",
                /^text\/plain/,
            );
            return await response.text();
        };
        assert.match(await metrics(), /agentd_processes_retained 0\n/);
        const process = await machine.startProcess({
            program: "/bin/sleep",
            args: ["30"],
        });
        try {
            const running = await metrics();
            assert.match(running, /agentd_processes_running 1\n/);
            assert.match(running, /agentd_processes_retained 1\n/);
        } finally {
            await process.kill();
            await process.wait();
        }
        const finished = await metrics();
        assert.match(finished, /agentd_processes_running 0\n/);
        assert.match(finished, /agentd_processes_retained 1\n/);
    }));
