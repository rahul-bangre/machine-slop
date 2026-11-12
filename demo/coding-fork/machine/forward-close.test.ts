import assert from "node:assert/strict";
import { Machine } from "../src/machine.ts";
import type { Forward } from "../src/forward.ts";

async function bounded<T>(promise: Promise<T>, ms = 1000): Promise<T> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
        return await Promise.race([
            promise,
            new Promise<never>((_, reject) => {
                timer = setTimeout(
                    () => reject(new Error("forward.close() did not settle")),
                    ms,
                );
            }),
        ]);
    } finally {
        clearTimeout(timer);
    }
}

function closeConnection(conn: Deno.Conn): void {
    try {
        conn.close();
    } catch (error) {
        if (!(error instanceof Deno.errors.BadResource)) throw error;
    }
}

async function pending(
    count: number,
    run: (
        forward: Forward,
        clients: Deno.TcpConn[],
        remotes: Deno.TcpConn[],
    ) => Promise<void>,
): Promise<void> {
    const listener = Deno.listen({ hostname: "127.0.0.1", port: 0 });
    const machine = new Machine(`http://127.0.0.1:${listener.addr.port}/`);
    const forward = machine.forward(3000);
    const clients: Deno.TcpConn[] = [];
    const remotes: Deno.TcpConn[] = [];
    try {
        for (let i = 0; i < count; i++) {
            clients.push(
                await Deno.connect({
                    hostname: "127.0.0.1",
                    port: forward.port,
                }),
            );
            const remote = await bounded(listener.accept());
            remotes.push(remote);
            let header = "";
            const bytes = new Uint8Array(4096);
            while (!header.includes("\r\n\r\n")) {
                const n = await bounded(remote.read(bytes));
                assert.notEqual(n, null);
                header += new TextDecoder().decode(bytes.subarray(0, n!));
            }
            assert.match(header, /Upgrade: tcp/i);
        }
        await run(forward, clients, remotes);
    } finally {
        for (const conn of [...clients, ...remotes]) closeConnection(conn);
        listener.close();
        await bounded(forward.close().catch(() => {}));
    }
}

Deno.test("closing a forward cancels an unanswered upgrade", () =>
    pending(1, async (forward, _clients, remotes) => {
        const closing = forward.close();
        closing.catch(() => {});
        await bounded(closing);
        assert.equal(await bounded(remotes[0]!.read(new Uint8Array(1))), null);
    }));

Deno.test("closing a forward cancels every pending upgrade", () =>
    pending(3, async (forward, _clients, remotes) => {
        const closing = forward.close();
        closing.catch(() => {});
        await bounded(closing);
        for (const remote of remotes) {
            assert.equal(await bounded(remote.read(new Uint8Array(1))), null);
        }
    }));

Deno.test("a client disconnect during setup still allows the forward to close", () =>
    pending(1, async (forward, clients, remotes) => {
        clients[0]!.close();
        const closing = forward.close();
        closing.catch(() => {});
        await bounded(closing);
        assert.equal(await bounded(remotes[0]!.read(new Uint8Array(1))), null);
    }));

Deno.test("an established forward still carries bytes in both directions", () =>
    pending(1, async (forward, clients, remotes) => {
        const client = clients[0]!;
        const remote = remotes[0]!;
        await remote.write(
            new TextEncoder().encode(
                "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n",
            ),
        );
        await client.write(new TextEncoder().encode("ping"));
        const data = new Uint8Array(4);
        assert.equal(await bounded(remote.read(data)), 4);
        assert.equal(new TextDecoder().decode(data), "ping");
        await remote.write(new TextEncoder().encode("pong"));
        assert.equal(await bounded(client.read(data)), 4);
        assert.equal(new TextDecoder().decode(data), "pong");
        await bounded(forward.close());
    }));
