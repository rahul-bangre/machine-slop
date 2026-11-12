import { Machine } from "/workspace/repo/packages/sdk/src/machine.ts";

const session = crypto.randomUUID();
const born = performance.now();
let phase = "ready";
let closed = false;
let started = 0;
let remote: Deno.TcpConn | null = null;
const upstream = Deno.listen({ hostname: "127.0.0.1", port: 0 });
const machine = new Machine(`http://127.0.0.1:${upstream.addr.port}/`);
const forward = machine.forward(3000);
let local: Deno.TcpConn | null = null;
async function reproduce() {
    if (phase !== "ready") return;
    phase = "connecting";
    local = await Deno.connect({ hostname: "127.0.0.1", port: forward.port });
    remote = await upstream.accept();
    const bytes = new Uint8Array(4096);
    await remote.read(bytes);
    phase = "cleanup_pending";
    started = performance.now();
    forward.close().then(() => {
        closed = true;
        phase = "closed";
    }).catch(() => {
        phase = "error";
    });
}

Deno.serve({ hostname: "127.0.0.1", port: 3001 }, async (req) => {
    const path = new URL(req.url).pathname;
    if (path === "/reproduce") {
        await reproduce();
        return Response.json({ phase });
    }
    if (path === "/state") {
        let tests = null;
        try {
            tests = JSON.parse(
                await Deno.readTextFile("/workspace/tests.json"),
            );
        } catch (error) {
            if (!(error instanceof Deno.errors.NotFound)) throw error;
        }
        return Response.json({
            connected: local !== null,
            session,
            pid: Deno.pid,
            phase,
            closed,
            uptime_ms: Math.round(performance.now() - born),
            waiting_ms: started ? Math.round(performance.now() - started) : 0,
            tests,
        });
    }
    return new Response(await Deno.readTextFile("/workspace/workspace.html"), {
        headers: { "content-type": "text/html" },
    });
});
console.log(JSON.stringify({ type: "workspace", session, pid: Deno.pid }));
