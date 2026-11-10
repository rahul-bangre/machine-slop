import assert from "node:assert/strict";

import {
    kill_grace_ms,
    machinedProvider,
    startAgentd,
} from "../src/agentd-sandbox.ts";
import { ProviderNotReady, SandboxError } from "../src/sandbox.ts";

type StubHost = {
    url: string;
    running: Set<string>;
    create_requests: unknown[];
    destroyed: string[];
    faults: { processes_fail: boolean; output_never_ends: boolean };
    stop(): Promise<void>;
};

async function withStubHost(run: (host: StubHost) => Promise<void>) {
    const agentd = await startAgentd("./target/debug/agentd");
    const running = new Set<string>();
    const create_requests: unknown[] = [];
    const destroyed: string[] = [];
    const faults = { processes_fail: false, output_never_ends: false };
    const hung_outputs: ReadableStreamDefaultController[] = [];
    const json = (body: unknown) => Response.json(body);

    const server = Deno.serve(
        { port: 0, hostname: "127.0.0.1", onListen: () => {} },
        async (request) => {
            const url = new URL(request.url);
            const [root, id, ...rest] = url.pathname.split("/").filter(Boolean);
            if (root !== "machines") return new Response(null, { status: 404 });
            if (!id && request.method === "POST") {
                create_requests.push(await request.json());
                const machine = crypto.randomUUID();
                running.add(machine);
                return json({
                    id: machine,
                    state: { state: "running" },
                    vcpus: 2,
                    memory_mib: 512,
                    boot_ms: 123,
                    allowed_hosts: [],
                });
            }
            if (!id) {
                return json([...running].map((machine) => ({ id: machine })));
            }
            if (rest.length === 0 && request.method === "DELETE") {
                running.delete(id);
                destroyed.push(id);
                return new Response(null, { status: 204 });
            }
            if (!running.has(id)) return new Response(null, { status: 404 });
            if (faults.processes_fail) {
                return new Response("guest is gone", { status: 502 });
            }
            if (faults.output_never_ends && rest.at(-1) === "kill") {
                return new Response("guest is hung", { status: 502 });
            }
            if (faults.output_never_ends && rest.at(-1) === "events") {
                return new Response(
                    new ReadableStream({
                        start: (controller) => {
                            hung_outputs.push(controller);
                        },
                    }),
                );
            }
            return await fetch(`${agentd.url}/${rest.join("/")}${url.search}`, {
                method: request.method,
                headers: request.headers,
                body: request.body,
            });
        },
    );
    const { port } = server.addr as Deno.NetAddr;
    try {
        await run({
            url: `http://127.0.0.1:${port}`,
            running,
            create_requests,
            destroyed,
            faults,
            stop: () => server.shutdown(),
        });
    } finally {
        for (const output of hung_outputs) output.close();
        await server.shutdown();
        await agentd.stop();
    }
}

Deno.test("a machined sandbox runs commands in its own machine, reports the host's boot time, and destroys the machine on close", () =>
    withStubHost(async (host) => {
        const provider = machinedProvider(host.url, "test-key");
        await provider.prepare();

        const sandbox = await provider.create();
        assert.equal(sandbox.reported_boot_ms, 123);
        assert.deepEqual(host.create_requests, [{ allowed_hosts: [] }]);
        assert.equal(host.running.size, 1);
        await sandbox.writeFile("nested/note.txt", "hello");
        const read = await sandbox.run("cat nested/note.txt", 10_000);
        assert.deepEqual([read.exit_code, read.stdout], [0, "hello"]);

        await sandbox.close();
        assert.equal(host.running.size, 0);
    }));

Deno.test("a machined sandbox is created with the machine settings it was given", () =>
    withStubHost(async (host) => {
        const machine = {
            vcpus: 2,
            memory_mib: 4096,
            allowed_hosts: ["registry.npmjs.org"],
        };
        const provider = machinedProvider(host.url, "test-key", machine);

        const sandbox = await provider.create();
        assert.deepEqual(host.create_requests, [machine]);
        assert.deepEqual(provider.describe().machine, machine);
        await sandbox.close();
    }));

Deno.test("machines the bench lost track of are destroyed before the next create, when setup fails, and when the run ends", () =>
    withStubHost(async (host) => {
        const provider = machinedProvider(host.url, "test-key");

        host.running.add("booted-after-a-lost-create-response");
        const sandbox = await provider.create();
        assert.deepEqual(host.destroyed, [
            "booted-after-a-lost-create-response",
        ]);
        assert.equal(host.running.size, 1);
        await sandbox.close();

        host.faults.processes_fail = true;
        await assert.rejects(provider.create());
        assert.equal(host.running.size, 0);
        host.faults.processes_fail = false;

        await provider.create();
        host.running.add("booted-after-an-interrupted-create");
        await provider.reclaim();
        assert.equal(host.running.size, 0);
    }));

Deno.test("a host that already runs machines is refused before the run starts", () =>
    withStubHost(async (host) => {
        host.running.add("someone-elses-machine");
        await assert.rejects(
            machinedProvider(host.url, "test-key").prepare(),
            ProviderNotReady,
        );
        assert.deepEqual(host.destroyed, []);

        await host.stop();
        await assert.rejects(
            machinedProvider(host.url, "test-key").prepare(),
            ProviderNotReady,
        );
    }));

Deno.test({
    name:
        "a command that cannot be killed fails the sandbox after the grace period instead of waiting forever",
    sanitizeOps: false,
    sanitizeResources: false,
    fn: () =>
        withStubHost(async (host) => {
            const provider = machinedProvider(host.url, "test-key");
            const sandbox = await provider.create();
            host.faults.output_never_ends = true;

            const started_at = performance.now();
            await assert.rejects(sandbox.run("sleep 60", 200), SandboxError);
            const waited_ms = performance.now() - started_at;
            assert.ok(waited_ms >= 200 + kill_grace_ms);
            assert.ok(waited_ms < 200 + kill_grace_ms + 2_000);

            host.faults.output_never_ends = false;
            await sandbox.close();
            assert.equal(host.running.size, 0);
        }),
});
