import { Host, type HostedMachine } from "../../../packages/sdk/src/mod.ts";
import type { Forward } from "../../../packages/sdk/src/forward.ts";
import { shell } from "../../common/shell.ts";

const home = Deno.env.get("HOME")!;
const key = (await Deno.readTextFile(`${home}/machine-slop/demo/api-key`))
    .trim();
const source_commit = Deno.env.get("SOURCE_COMMIT") ?? "0d85366";
const host = new Host("http://127.0.0.1:4100", { apiKey: key });
const run_dir = Deno.args[0] ?? `${home}/machine-slop/demo/runs/coding-fork`;
await Deno.mkdir(run_dir, { recursive: true });
const log = await Deno.open(`${run_dir}/events.jsonl`, {
    create: true,
    write: true,
    truncate: true,
});
const started = performance.now();
const encoder = new TextEncoder();
const machines = new Map<string, HostedMachine>();
const forwards = new Map<string, Forward>();
let snapshot_id: string | null = null;
let source_state: Record<string, unknown> | null = null;
const ports: Record<string, number> = {
    origin: 4280,
    A: 4281,
    B: 4282,
    C: 4283,
};
function emit(type: string, data: Record<string, unknown> = {}) {
    const event = { ...data, type, t: Math.round(performance.now() - started) };
    log.writeSync(encoder.encode(JSON.stringify(event) + "\n"));
    console.log(JSON.stringify(event).slice(0, 200));
}
async function exec(lane: string, command: string, timeout = 120_000) {
    const machine = machines.get(lane);
    if (!machine) throw new Error(`Unknown lane ${lane}`);
    emit("command_start", { lane, command });
    const result = await shell(machine, command, timeout, "/workspace/repo");
    emit("command", { lane, command, ...result });
    const counts = result.output.match(/(\d+) passed \| (\d+) failed/);
    if (counts) {
        const summary = `${counts[1]} passed · ${counts[2]} failed`;
        await machine.writeFile(
            "/workspace/tests.json",
            JSON.stringify({
                summary,
                passed: Number(counts[1]),
                failed: Number(counts[2]),
            }),
        );
        emit("tests", {
            lane,
            passed: Number(counts[1]),
            failed: Number(counts[2]),
            summary,
        });
    }
    return result;
}
async function connect(lane: string, machine: HostedMachine) {
    const forward = machine.forward(3001, ports[lane]);
    forwards.set(lane, forward);
    return await (await fetch(`${forward.url}/state`)).json();
}
async function setup() {
    const begun = performance.now();
    const created = await host.createMachine({
        memory_mib: 384,
        vcpus: 2,
        idle_timeout_secs: 3600,
        allowed_hosts: [],
    });
    machines.set("origin", created.machine);
    emit("machine", {
        lane: "origin",
        id: created.machine.id,
        ms: Math.round(performance.now() - begun),
    });
    await created.machine.writeArchive(
        "/workspace/repo",
        (await new Deno.Command("git", {
            args: ["archive", "--format=tar", source_commit],
            stdout: "piped",
        }).output()).stdout,
    );
    await created.machine.writeFile(
        "/usr/local/bin/deno",
        await Deno.readFile(`${home}/.deno/bin/deno`),
        { mode: 0o755 },
    );
    await created.machine.writeFile(
        "/workspace/repo/target/debug/agentd",
        await Deno.readFile(
            `${home}/projects/machine-slop/target/debug/agentd`,
        ),
        { mode: 0o755 },
    );
    await exec(
        "origin",
        "git init -q && git add -A && git -c user.name='Machine Slop Demo' -c user.email='demo@slop.dev' commit -qm 'Captured source baseline' && deno --version",
    );
    await created.machine.writeFile(
        "/workspace/repo/packages/sdk/test/forward-close.test.ts",
        await Deno.readFile(
            new URL("../machine/forward-close.test.ts", import.meta.url),
        ),
    );
    for (const name of ["workspace.ts", "workspace.html"]) {
        await created.machine.writeFile(
            `/workspace/${name}`,
            await Deno.readFile(new URL(`../machine/${name}`, import.meta.url)),
        );
    }
    const process = await created.machine.startProcess({
        program: "/usr/local/bin/deno",
        args: [
            "run",
            "--inspect=127.0.0.1:9229",
            "--no-config",
            "-A",
            "/workspace/workspace.ts",
        ],
        cwd: "/workspace/repo",
    });
    emit("server", { lane: "origin", process_id: process.id });
    let last: unknown;
    for (let n = 0; n < 40; n++) {
        try {
            source_state = await connect("origin", created.machine);
            break;
        } catch (error) {
            last = error;
            await forwards.get("origin")?.close().catch(() => {});
            forwards.delete("origin");
            await new Promise((resolve) => setTimeout(resolve, 100));
        }
    }
    if (!source_state) throw last;
    await fetch("http://127.0.0.1:4280/reproduce", { method: "POST" });
    source_state = await (await fetch("http://127.0.0.1:4280/state")).json();
    emit("reproduced", { lane: "origin", ...source_state });
    await exec(
        "origin",
        "deno test -A packages/sdk/test/forward-close.test.ts",
    );
    return source_state;
}
async function fork() {
    const source = machines.get("origin")!;
    await forwards.get("origin")!.close();
    forwards.delete("origin");
    const begun = performance.now();
    emit("pause_start");
    const pause = performance.now();
    await source.pause();
    emit("paused", { ms: Math.round(performance.now() - pause) });
    const snap = performance.now();
    const snapshot = await source.snapshot();
    snapshot_id = snapshot.id;
    emit("snapshot", {
        id: snapshot.id,
        ms: Math.round(performance.now() - snap),
    });
    const start = performance.now();
    const results = await Promise.allSettled(
        ["A", "B", "C"].map(async (lane) => {
            const made = await host.createMachine({
                snapshot_id: snapshot.id,
                idle_timeout_secs: 3600,
                allowed_hosts: [],
            });
            machines.set(lane, made.machine);
            const state = await connect(lane, made.machine);
            if (
                state.session !== source_state?.session ||
                state.pid !== source_state?.pid ||
                state.phase !== "cleanup_pending"
            ) {
                throw new Error(
                    `Lane ${lane} did not inherit the reproduction`,
                );
            }
            emit("copy", {
                lane,
                id: made.machine.id,
                state,
                ms: Math.round(performance.now() - start),
            });
            return { lane, id: made.machine.id, port: ports[lane], state };
        }),
    );
    const failed = results.find((r) => r.status === "rejected");
    if (failed?.status === "rejected") throw failed.reason;
    emit("forked", {
        copies: 3,
        ms: Math.round(performance.now() - start),
        total_ms: Math.round(performance.now() - begun),
    });
    return results.map((r) => r.status === "fulfilled" ? r.value : null);
}
async function collect(lane: string) {
    const result = await exec(
        lane,
        "git add -N packages/sdk/test/forward-close.test.ts && git diff --binary -- packages/sdk/src packages/sdk/test/forward-close.test.ts",
    );
    await Deno.writeTextFile(`${run_dir}/${lane}.patch`, result.output);
    const info = await exec(lane, "git diff --numstat && git status --short");
    const state = await (await fetch(`http://127.0.0.1:${ports[lane]}/state`))
        .json();
    emit("collected", { lane, state, diff: info.output });
    return { patch: `${run_dir}/${lane}.patch`, diff: info.output, state };
}
async function cleanup() {
    for (const forward of forwards.values()) {
        await forward.close().catch(() => {});
    }
    forwards.clear();
    for (const machine of machines.values()) {
        await machine.destroy().catch(() => {});
    }
    machines.clear();
    if (snapshot_id) await host.deleteSnapshot(snapshot_id).catch(() => {});
    snapshot_id = null;
    emit("cleanup");
}
const server = Deno.serve(
    { hostname: "127.0.0.1", port: 4300 },
    async (req) => {
        try {
            const path = new URL(req.url).pathname;
            const body = req.method === "POST" ? await req.json() : {};
            if (path === "/setup") return Response.json(await setup());
            if (path === "/fork") return Response.json(await fork());
            if (path === "/exec") {
                return Response.json(
                    await exec(String(body.lane), String(body.command)),
                );
            }
            if (path === "/collect") {
                return Response.json(
                    await collect(String(body.lane)),
                );
            }
            if (path === "/event") {
                emit(String(body.type), body);
                return Response.json({ ok: true });
            }
            if (path === "/cleanup") {
                await cleanup();
                return Response.json({ ok: true });
            }
            return Response.json({ machines: [...machines.keys()], run_dir });
        } catch (error) {
            emit("error", { message: String(error) });
            return Response.json({ error: String(error) }, { status: 500 });
        }
    },
);
Deno.addSignalListener("SIGTERM", async () => {
    await cleanup();
    await server.shutdown();
    log.close();
});
