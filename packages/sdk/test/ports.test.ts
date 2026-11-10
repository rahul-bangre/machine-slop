import assert from "node:assert/strict";

import { connectPreview, HttpError } from "../src/mod.ts";
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

async function readUntilEnd(
    conn: Deno.Conn,
    limit = Infinity,
): Promise<string> {
    const decoder = new TextDecoder();
    const buffer = new Uint8Array(1024);
    let text = "";
    let count;
    while (text.length < limit && (count = await conn.read(buffer)) !== null) {
        text += decoder.decode(buffer.subarray(0, count), { stream: true });
    }
    return text;
}

Deno.test("a tunnel carries bytes both ways and ends when the port closes", () =>
    withMachine(async (machine) => {
        const listener = Deno.listen({ hostname: "127.0.0.1", port: 0 });
        const served = (async () => {
            const conn = await listener.accept();
            const request = await readUntilEnd(conn, 3);
            await conn.write(
                new TextEncoder().encode([...request].reverse().join("")),
            );
            conn.close();
        })();
        try {
            const tunnel = await machine.connect(listener.addr.port);
            const writer = tunnel.writable.getWriter();
            await writer.write(new TextEncoder().encode("abc"));
            assert.equal(await new Response(tunnel.readable).text(), "cba");
            await served;
            tunnel.close();
        } finally {
            listener.close();
        }
    }));

Deno.test("closing the client side of a tunnel ends the port's connection", () =>
    withMachine(async (machine) => {
        const listener = Deno.listen({ hostname: "127.0.0.1", port: 0 });
        const received = (async () => {
            const conn = await listener.accept();
            const text = await readUntilEnd(conn);
            conn.close();
            return text;
        })();
        try {
            const tunnel = await machine.connect(listener.addr.port);
            const writer = tunnel.writable.getWriter();
            await writer.write(new TextEncoder().encode("sent"));
            await writer.close();
            assert.equal(await received, "sent");
            await tunnel.readable.cancel();
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

Deno.test("a client's bytes reach a port that is writing when the client ends", () =>
    withMachine(async (machine) => {
        const listener = Deno.listen({ hostname: "127.0.0.1", port: 0 });
        const upload = new Uint8Array(4 * 1024 * 1024).fill(7);
        try {
            for (let attempt = 0; attempt < 10; attempt++) {
                const received = (async () => {
                    const conn = await listener.accept();
                    const chatter = (async () => {
                        const chunk = new Uint8Array(4096);
                        try {
                            while (true) await conn.write(chunk);
                        } catch {
                            return;
                        }
                    })();
                    const buffer = new Uint8Array(64 * 1024);
                    let total = 0;
                    let count;
                    while ((count = await conn.read(buffer)) !== null) {
                        total += count;
                    }
                    conn.close();
                    await chatter;
                    return total;
                })();
                const tunnel = await machine.connect(listener.addr.port);
                const draining = tunnel.readable.pipeTo(new WritableStream())
                    .catch(() => {});
                const writer = tunnel.writable.getWriter();
                await writer.write(upload);
                await writer.close();
                assert.equal(
                    await received,
                    upload.byteLength,
                    `attempt ${attempt}`,
                );
                await draining;
                tunnel.close();
            }
        } finally {
            listener.close();
        }
    }));

Deno.test("connectPreview asks the preview origin for a tcp upgrade at its root", async () => {
    const listener = Deno.listen({ hostname: "127.0.0.1", port: 0 });
    const served = (async () => {
        const conn = await listener.accept();
        try {
            const decoder = new TextDecoder();
            const buffer = new Uint8Array(1024);
            let head = "";
            while (!head.includes("\r\n\r\n")) {
                const count = await conn.read(buffer);
                if (count === null) break;
                head += decoder.decode(buffer.subarray(0, count));
            }
            await conn.write(
                new TextEncoder().encode(
                    "HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: tcp\r\n\r\nraw bytes",
                ),
            );
            return head;
        } finally {
            conn.close();
        }
    })();
    try {
        const origin = `http://127.0.0.1:${listener.addr.port}`;
        const tunnel = await connectPreview(`${origin}/ignored/path`);
        const reader = tunnel.readable.getReader();
        let received = "";
        for (let chunk; !(chunk = await reader.read()).done;) {
            received += new TextDecoder().decode(chunk.value);
        }
        tunnel.close();
        const head = await served;
        assert.match(head, /^GET \/ HTTP\/1\.1\r\n/);
        assert.match(
            head,
            new RegExp(`Host: 127\\.0\\.0\\.1:${listener.addr.port}\r\n`),
        );
        assert.match(head, /Upgrade: tcp\r\n/);
        assert.equal(received, "raw bytes");
    } finally {
        listener.close();
    }
});
