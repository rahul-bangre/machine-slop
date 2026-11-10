import assert from "node:assert/strict";

import { Host } from "../src/mod.ts";

const machine_info = {
    id: "m1",
    state: { state: "running" },
    vcpus: 1,
    memory_mib: 256,
    boot_ms: 400,
    idle_timeout_secs: 600,
};

const finished = {
    type: "finished",
    seq: 3,
    result: {
        kind: "exited",
        code: 0,
        signal: null,
        output_complete: true,
        stdin_complete: true,
    },
};

const routes: Record<string, () => Response> = {
    "POST /base/machines": () => Response.json(machine_info, { status: 201 }),
    "POST /base/machines/m1/processes": () =>
        Response.json({ id: "p1", status: { state: "running" } }, {
            status: 201,
        }),
    "GET /base/machines/m1/processes/p1/events?cursor=3": () =>
        new Response(`${JSON.stringify(finished)}\n`),
    "GET /base/machines/m1": () => Response.json(machine_info),
    "DELETE /base/machines/m1": () => new Response(null, { status: 204 }),
};

Deno.test("a hosted machine addresses its agent under the host's machine path", async () => {
    const seen: string[] = [];
    const server = Deno.serve(
        { hostname: "127.0.0.1", port: 0, onListen() {} },
        async (request) => {
            const url = new URL(request.url);
            const route = `${request.method} ${url.pathname}${url.search}`;
            seen.push(`${route} ${await request.text()}`.trimEnd());
            return routes[route]?.() ??
                new Response("unexpected", { status: 404 });
        },
    );
    try {
        const host = new Host(`http://127.0.0.1:${server.addr.port}/base`);
        const { machine, info } = await host.createMachine({
            vcpus: 1,
            memory_mib: 256,
        });
        assert.deepEqual(info, machine_info);
        const process = await machine.startProcess({ program: "/bin/true" });
        assert.deepEqual(await Array.fromAsync(process.events(3)), [finished]);
        assert.deepEqual(await machine.info(), machine_info);
        await machine.destroy();
        assert.deepEqual(seen, [
            'POST /base/machines {"vcpus":1,"memory_mib":256}',
            'POST /base/machines/m1/processes {"program":"/bin/true"}',
            "GET /base/machines/m1/processes/p1/events?cursor=3",
            "GET /base/machines/m1",
            "DELETE /base/machines/m1",
        ]);
    } finally {
        await server.shutdown();
    }
});
