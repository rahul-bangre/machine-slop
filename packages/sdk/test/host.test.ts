import assert from "node:assert/strict";

import { Host } from "../src/mod.ts";

const machine_info = {
    id: "m1",
    state: { state: "running" },
    vcpus: 1,
    memory_mib: 256,
    boot_ms: 400,
    idle_timeout_secs: 600,
    allowed_hosts: [],
    credential_hosts: [],
    egress_address: null,
};

const preview_info = {
    id: "v1",
    machine_id: "m1",
    port: 8000,
    url: "https://v1.preview.example.com/",
    expires_unix_ms: 1794294079990,
};

const snapshot_info = {
    id: "s1",
    machine_id: "m1",
    vcpus: 1,
    memory_mib: 256,
    allowed_hosts: [],
    created_unix_ms: 1794294079990,
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

const paused_info = { ...machine_info, state: { state: "paused" } };

const usage = {
    tenant: "acme",
    quotas: {
        running_machines: 4,
        memory_mib: 4096,
        disk_gib: 100,
        creates_per_second: 2,
    },
    vcpu_ms: 2000,
    memory_mib_ms: 512000,
    snapshot_mib_ms: 0,
    as_of_unix_ms: 1794294079990,
};

const routes: Record<string, () => Response> = {
    "POST /base/machines": () => Response.json(machine_info, { status: 201 }),
    "POST /base/machines/m1/pause": () => Response.json(paused_info),
    "POST /base/machines/m1/resume": () => Response.json(machine_info),
    "POST /base/snapshots": () => Response.json(snapshot_info, { status: 201 }),
    "POST /base/machines/m1/fork": () =>
        Response.json({ ...machine_info, id: "m2" }, { status: 201 }),
    "GET /base/snapshots": () => Response.json([snapshot_info]),
    "GET /base/snapshots/s1": () => Response.json(snapshot_info),
    "DELETE /base/snapshots/s1": () => new Response(null, { status: 204 }),
    "POST /base/machines/m1/processes": () =>
        Response.json({ id: "p1", status: { state: "running" } }, {
            status: 201,
        }),
    "GET /base/machines/m1/processes/p1/events?cursor=3": () =>
        new Response(`${JSON.stringify(finished)}\n`),
    "GET /base/machines/m1": () => Response.json(machine_info),
    "PATCH /base/machines/m1": () =>
        Response.json({ ...machine_info, allowed_hosts: ["pypi.org"] }),
    "DELETE /base/machines/m1": () => new Response(null, { status: 204 }),
    "POST /base/previews": () => Response.json(preview_info, { status: 201 }),
    "GET /base/previews": () => Response.json([preview_info]),
    "DELETE /base/previews/v1": () => new Response(null, { status: 204 }),
    "GET /base/usage": () => Response.json(usage),
};

Deno.test("a hosted machine addresses its agent under the host's machine path", async () => {
    const seen: string[] = [];
    const authorized: string[] = [];
    const server = Deno.serve(
        { hostname: "127.0.0.1", port: 0, onListen() {} },
        async (request) => {
            const url = new URL(request.url);
            const route = `${request.method} ${url.pathname}${url.search}`;
            const key = request.headers.get("idempotency-key");
            seen.push(
                [route, await request.text(), key ? `key=${key}` : ""]
                    .filter((part) => part !== "").join(" "),
            );
            authorized.push(request.headers.get("authorization") ?? "");
            return routes[route]?.() ??
                new Response("unexpected", { status: 404 });
        },
    );
    try {
        const host = new Host(`http://127.0.0.1:${server.addr.port}/base`, {
            apiKey: "secret",
        });
        const { machine, info } = await host.createMachine(
            {
                vcpus: 1,
                memory_mib: 256,
            },
            undefined,
            "create-1",
        );
        assert.deepEqual(info, machine_info);
        const process = await machine.startProcess({ program: "/bin/true" });
        assert.deepEqual(await Array.fromAsync(process.events(3)), [finished]);
        assert.deepEqual(await machine.info(), machine_info);
        assert.deepEqual(
            (await machine.setAllowedHosts(["pypi.org"])).allowed_hosts,
            ["pypi.org"],
        );
        await machine.setEgressCredentials([{
            host: "api.example.com",
            headers: { authorization: "Bearer key" },
        }]);
        assert.deepEqual(
            await machine.preview(8000, { ttl_secs: 60 }),
            preview_info,
        );
        assert.deepEqual(await host.previews(), [preview_info]);
        await host.deletePreview("v1");
        assert.deepEqual(await machine.pause(), paused_info);
        assert.deepEqual(
            await machine.snapshot(undefined, "snapshot-1"),
            snapshot_info,
        );
        assert.deepEqual(await machine.resume(), machine_info);
        const fork = await machine.fork(undefined, "fork-1");
        assert.equal(fork.machine.id, "m2");
        assert.equal(fork.info.id, "m2");
        assert.deepEqual(await host.snapshots(), [snapshot_info]);
        assert.deepEqual(await host.snapshot("s1"), snapshot_info);
        const restored = await host.createMachine({ snapshot_id: "s1" });
        assert.equal(restored.machine.id, "m1");
        await host.deleteSnapshot("s1");
        await machine.destroy();
        assert.deepEqual(await host.usage(), usage);
        assert.deepEqual(seen, [
            'POST /base/machines {"vcpus":1,"memory_mib":256} key=create-1',
            'POST /base/machines/m1/processes {"program":"/bin/true"}',
            "GET /base/machines/m1/processes/p1/events?cursor=3",
            "GET /base/machines/m1",
            'PATCH /base/machines/m1 {"allowed_hosts":["pypi.org"]}',
            'PATCH /base/machines/m1 {"egress_credentials":[{"host":"api.example.com","headers":{"authorization":"Bearer key"}}]}',
            'POST /base/previews {"machine_id":"m1","port":8000,"ttl_secs":60}',
            "GET /base/previews",
            "DELETE /base/previews/v1",
            "POST /base/machines/m1/pause",
            'POST /base/snapshots {"machine_id":"m1"} key=snapshot-1',
            "POST /base/machines/m1/resume",
            "POST /base/machines/m1/fork key=fork-1",
            "GET /base/snapshots",
            "GET /base/snapshots/s1",
            'POST /base/machines {"snapshot_id":"s1"}',
            "DELETE /base/snapshots/s1",
            "DELETE /base/machines/m1",
            "GET /base/usage",
        ]);
        assert.ok(authorized.every((header) => header === "Bearer secret"));
    } finally {
        await server.shutdown();
    }
});

Deno.test("a hosted machine's port tunnel carries the host's API key", async () => {
    const listener = Deno.listen({ hostname: "127.0.0.1", port: 0 });
    const accepted = (async () => {
        const conn = await listener.accept();
        const buffer = new Uint8Array(4096);
        let head = "";
        while (!head.includes("\r\n\r\n")) {
            const read = await conn.read(buffer);
            if (read === null) break;
            head += new TextDecoder().decode(buffer.subarray(0, read));
        }
        await conn.write(
            new TextEncoder().encode(
                "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n",
            ),
        );
        conn.close();
        return head;
    })();
    try {
        const host = new Host(`http://127.0.0.1:${listener.addr.port}`, {
            apiKey: "secret",
        });
        const tunnel = await host.machine("m1").connect(8080);
        tunnel.close();
        const head = await accepted;
        assert.match(head, /^GET \/machines\/m1\/ports\/8080 HTTP\/1.1\r\n/);
        assert.match(head, /\r\nauthorization: Bearer secret\r\n/);
    } finally {
        listener.close();
    }
});
