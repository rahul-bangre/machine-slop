import assert from "node:assert/strict";

import { HttpError } from "../src/mod.ts";
import { withMachine } from "./agentd.ts";

async function withServer(
    handler: Deno.ServeHandler,
    run: (port: number) => Promise<void>,
): Promise<void> {
    const server = Deno.serve(
        { hostname: "127.0.0.1", port: 0, onListen() {} },
        handler,
    );
    try {
        await run(server.addr.port);
    } finally {
        await server.shutdown();
    }
}

function unusedPort(): number {
    const listener = Deno.listen({ hostname: "127.0.0.1", port: 0 });
    listener.close();
    return listener.addr.port;
}

Deno.test("a tunnel carries bytes both ways and delivers a half-close", () =>
    withMachine(async (machine) => {
        const listener = Deno.listen({ hostname: "127.0.0.1", port: 0 });
        const echoed = (async () => {
            const conn = await listener.accept();
            const received = new Uint8Array(
                await new Response(
                    ReadableStream.from(
                        (async function* () {
                            const buffer = new Uint8Array(1024);
                            let count;
                            while ((count = await conn.read(buffer)) !== null) {
                                yield buffer.slice(0, count);
                            }
                        })(),
                    ),
                ).arrayBuffer(),
            );
            await conn.write(received.reverse());
            conn.close();
        })();
        try {
            const tunnel = await machine.connect(listener.addr.port);
            const writer = tunnel.writable.getWriter();
            await writer.write(new TextEncoder().encode("abc"));
            await writer.close();
            assert.equal(await new Response(tunnel.readable).text(), "cba");
            await echoed;
        } finally {
            listener.close();
        }
    }));

Deno.test("a forward serves a machine port on a local address", () =>
    withMachine((machine) =>
        withServer(
            (request) =>
                new Response(`served ${new URL(request.url).pathname}`),
            async (port) => {
                const forward = machine.forward(port);
                try {
                    const responses = await Promise.all(
                        ["/a", "/b", "/c"].map(async (path) =>
                            (await fetch(`${forward.url}${path}`)).text()
                        ),
                    );
                    assert.deepEqual(responses, [
                        "served /a",
                        "served /b",
                        "served /c",
                    ]);
                } finally {
                    await forward.close();
                }
            },
        )
    ));

Deno.test("a port with no listener is a gateway error, and plain requests need an upgrade", () =>
    withMachine(async (machine, _terminate, url) => {
        await assert.rejects(
            machine.connect(unusedPort()),
            (error) => error instanceof HttpError && error.status === 502,
        );
        await assert.rejects(machine.connect(0), RangeError);
        const response = await fetch(new URL("/ports/80", url));
        assert.equal(response.status, 426);
        await response.body?.cancel();
    }));
