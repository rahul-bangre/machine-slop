import assert from "node:assert/strict";

import { HttpError, tar } from "../src/mod.ts";
import { withMachine } from "./agentd.ts";

const status = (code: number) => (error: unknown) =>
    error instanceof HttpError && error.status === code;

async function inTempDir(run: (dir: string) => Promise<void>): Promise<void> {
    const dir = await Deno.realPath(await Deno.makeTempDir());
    try {
        await run(dir);
    } finally {
        await Deno.remove(dir, { recursive: true });
    }
}

async function rawRequest(url: string, request_line: string): Promise<string> {
    const { hostname, port } = new URL(url);
    const conn = await Deno.connect({ hostname, port: Number(port) });
    await conn.write(
        new TextEncoder().encode(
            `${request_line} HTTP/1.1\r\nHost: agentd\r\nConnection: close\r\n\r\n`,
        ),
    );
    return await new Response(conn.readable).text();
}

function bytes(length: number): Uint8Array<ArrayBuffer> {
    return Uint8Array.from({ length }, (_, i) => (i * 7919) % 256);
}

Deno.test("files stream raw bytes of any size, create parents, and keep or set modes", () =>
    withMachine((machine) =>
        inTempDir(async (dir) => {
            const big = bytes(5 * 1024 * 1024 + 3);
            const path = `${dir}/nested deeper/ünïcode #?.bin`;
            await machine.writeFile(path, big);
            assert.deepEqual(await machine.readFile(path), big);
            assert.equal((await Deno.stat(path)).mode! & 0o7777, 0o644);

            await machine.writeFile(`${dir}/run.sh`, "#!/bin/sh\n", {
                mode: 0o750,
            });
            await machine.writeFile(`${dir}/run.sh`, "#!/bin/sh\necho\n");
            assert.equal(
                (await Deno.stat(`${dir}/run.sh`)).mode! & 0o7777,
                0o750,
            );
            assert.equal(
                await Deno.readTextFile(`${dir}/run.sh`),
                "#!/bin/sh\necho\n",
            );

            const streamed = ReadableStream.from([bytes(10), bytes(20)]);
            await machine.writeFile(`${dir}/streamed`, streamed);
            assert.equal(
                (await machine.readFile(`${dir}/streamed`)).length,
                30,
            );
            assert.deepEqual(
                [...await Array.fromAsync(Deno.readDir(dir))]
                    .map((entry) => entry.name).sort(),
                ["nested deeper", "run.sh", "streamed"],
            );
        })
    ));

Deno.test("an interrupted upload leaves the previous file and no staging file", () =>
    withMachine((machine) =>
        inTempDir(async (dir) => {
            const path = `${dir}/kept`;
            await machine.writeFile(path, "previous");
            const controller = new AbortController();
            const sent = Promise.withResolvers<void>();
            const body = new ReadableStream<Uint8Array<ArrayBuffer>>({
                async pull(stream) {
                    stream.enqueue(bytes(64 * 1024));
                    sent.resolve();
                    await new Promise((resolve) => setTimeout(resolve, 10));
                },
            });
            const writing = machine.writeFile(path, body, {
                signal: controller.signal,
            });
            await sent.promise;
            controller.abort();
            await assert.rejects(writing, { name: "AbortError" });
            const deadline = Date.now() + 5_000;
            while (true) {
                const names = (await Array.fromAsync(Deno.readDir(dir)))
                    .map((entry) => entry.name);
                if (names.length === 1) {
                    assert.deepEqual(names, ["kept"]);
                    break;
                }
                assert.ok(Date.now() < deadline, `staging file left: ${names}`);
                await new Promise((resolve) => setTimeout(resolve, 20));
            }
            assert.equal(await Deno.readTextFile(path), "previous");
        })
    ));

Deno.test("file requests report missing paths, directories, and invalid paths explicitly", () =>
    withMachine((machine, _terminate, url) =>
        inTempDir(async (dir) => {
            await assert.rejects(
                machine.readFile(`${dir}/missing`),
                status(404),
            );
            await assert.rejects(machine.readFile(dir), status(409));
            await assert.rejects(machine.writeFile(dir, "x"), status(409));
            await assert.rejects(
                machine.writeFile(`${dir}/x`, "x", { mode: 0o10000 }),
                status(400),
            );
            for (const path of ["relative", `${dir}/../etc`, `${dir}/`, "/"]) {
                await assert.rejects(machine.readFile(path), TypeError);
            }
            assert.match(
                await rawRequest(url, `GET /files${dir}/%2E%2E/etc/hostname`),
                /^HTTP\/1.1 400 /,
            );
            await Deno.mkdir(`${dir}/tree/inner`, { recursive: true });
            await Deno.writeTextFile(`${dir}/tree/inner/file`, "x");
            await machine.remove(`${dir}/tree`);
            await machine.remove(`${dir}/tree`);
            await assert.rejects(
                Deno.stat(`${dir}/tree`),
                Deno.errors.NotFound,
            );
        })
    ));

Deno.test("archives write many files in one request and read a directory back", () =>
    withMachine((machine) =>
        inTempDir(async (dir) => {
            const long_name = `${"d".repeat(60)}/${"f".repeat(70)}.txt`;
            await machine.writeArchive(
                `${dir}/project`,
                tar({
                    "README.md": "hello",
                    "bin/run": { data: "#!/bin/sh\n", mode: 0o755 },
                    "data/blob": bytes(70_000),
                    [long_name]: "long",
                }),
            );
            assert.equal(
                await Deno.readTextFile(`${dir}/project/README.md`),
                "hello",
            );
            assert.equal(
                (await Deno.stat(`${dir}/project/bin/run`)).mode! & 0o7777,
                0o755,
            );
            assert.deepEqual(
                await Deno.readFile(`${dir}/project/data/blob`),
                bytes(70_000),
            );
            assert.equal(
                await Deno.readTextFile(`${dir}/project/${long_name}`),
                "long",
            );

            await Deno.symlink("/etc", `${dir}/project/link`);
            await machine.writeArchive(
                `${dir}/copy`,
                await machine.readArchive(`${dir}/project`),
            );
            assert.equal(await Deno.readLink(`${dir}/copy/link`), "/etc");
            assert.deepEqual(
                await Deno.readFile(`${dir}/copy/data/blob`),
                bytes(70_000),
            );
            assert.equal(
                (await Deno.stat(`${dir}/copy/bin/run`)).mode! & 0o7777,
                0o755,
            );
            await assert.rejects(
                machine.readArchive(`${dir}/project/README.md`),
                status(409),
            );
        })
    ));

Deno.test("an archive entry that leaves its directory is rejected", () =>
    withMachine((machine) =>
        inTempDir(async (dir) => {
            const archive = new Uint8Array(
                await tar({ "xx/escaped": "no" }).arrayBuffer(),
            );
            archive.set(new TextEncoder().encode(".."), 0);
            const checksum = archive.subarray(0, 512).reduce(
                (sum, byte, i) => sum + (i >= 148 && i < 156 ? 0x20 : byte),
                0,
            );
            archive.set(
                new TextEncoder().encode(
                    `${checksum.toString(8).padStart(6, "0")}\u0000 `,
                ),
                148,
            );
            await assert.rejects(
                machine.writeArchive(`${dir}/target`, archive),
                status(422),
            );
            await assert.rejects(
                Deno.stat(`${dir}/escaped`),
                Deno.errors.NotFound,
            );
            assert.throws(() => tar({ "../escaped": "no" }), TypeError);
            assert.throws(() => tar({ "/absolute": "no" }), TypeError);
        })
    ));
