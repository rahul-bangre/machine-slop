import assert from "node:assert/strict";

import { HttpError } from "@machine-slop/sdk";

import { machinedSandboxes } from "../src/machined.ts";
import { ProviderNotReady } from "../src/provider.ts";

const resources = { vcpus: 2, memory_mib: 4096 };

type StubHost = {
    url: string;
    running: Set<string>;
    create_requests: unknown[];
    destroy_status: number;
};

async function withStubHost(run: (host: StubHost) => Promise<void>) {
    const host: StubHost = {
        url: "",
        running: new Set(),
        create_requests: [],
        destroy_status: 204,
    };
    const server = Deno.serve(
        { hostname: "127.0.0.1", port: 0, onListen: () => {} },
        async (request) => {
            const [root, id] = new URL(request.url).pathname.split("/")
                .filter(Boolean);
            if (root !== "machines") return new Response(null, { status: 404 });
            if (!id && request.method === "GET") {
                return Response.json(
                    [...host.running].map((machine) => ({ id: machine })),
                );
            }
            if (!id && request.method === "POST") {
                const create = await request.json();
                host.create_requests.push(create);
                const machine = `machine-${host.create_requests.length}`;
                host.running.add(machine);
                return Response.json({
                    id: machine,
                    state: { state: "running" },
                    vcpus: create.vcpus,
                    memory_mib: create.memory_mib,
                    boot_ms: 100,
                    idle_timeout_secs: create.idle_timeout_secs,
                    allowed_hosts: create.allowed_hosts,
                }, { status: 201 });
            }
            if (id && request.method === "DELETE") {
                if (host.destroy_status === 204) host.running.delete(id);
                return new Response(null, { status: host.destroy_status });
            }
            return new Response(null, { status: 405 });
        },
    );
    try {
        host.url = `http://127.0.0.1:${server.addr.port}`;
        await run(host);
    } finally {
        await server.shutdown();
    }
}

Deno.test("a machined host that already runs machines is refused before any machine is created", () =>
    withStubHost(async (host) => {
        host.running.add("someone-elses-machine");

        await assert.rejects(
            machinedSandboxes(host.url, "test-key", resources),
            ProviderNotReady,
        );
        assert.deepEqual(host.create_requests, []);
        assert.deepEqual([...host.running], ["someone-elses-machine"]);
    }));

Deno.test("an unreachable machined is reported as not ready", async () => {
    const listener = Deno.listen({ hostname: "127.0.0.1", port: 0 });
    const { port } = listener.addr;
    listener.close();

    await assert.rejects(
        machinedSandboxes(`http://127.0.0.1:${port}`, "test-key", resources),
        ProviderNotReady,
    );
});

Deno.test("a smoke machine has the pinned resources, no network, and an idle expiry", () =>
    withStubHost(async (host) => {
        const sandboxes = await machinedSandboxes(
            host.url,
            "test-key",
            resources,
        );

        const guest = await sandboxes.start({ "env-smoke-run": "run-1" });

        assert.deepEqual(host.create_requests, [{
            vcpus: 2,
            memory_mib: 4096,
            idle_timeout_secs: 900,
            allowed_hosts: [],
        }]);
        assert.deepEqual(guest.reported, resources);
        assert.deepEqual(await sandboxes.owned({}), [guest.id]);
        await sandboxes.destroy(guest.id);
        assert.deepEqual(await sandboxes.owned({}), []);
    }));

Deno.test("a machine that is already gone counts as destroyed, and any other refusal does not", () =>
    withStubHost(async (host) => {
        const sandboxes = await machinedSandboxes(
            host.url,
            "test-key",
            resources,
        );

        host.destroy_status = 404;
        await sandboxes.destroy("expired-machine");

        host.destroy_status = 502;
        await assert.rejects(
            sandboxes.destroy("unreachable-machine"),
            (error) => error instanceof HttpError && error.status === 502,
        );
    }));
