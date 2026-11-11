import { Host, type HostedMachine } from "../../../packages/sdk/src/mod.ts";
import type { Process } from "../../../packages/sdk/src/process.ts";
import { delay } from "../../common/browser.ts";

const home = Deno.env.get("HOME")!;
const machined_url = Deno.env.get("MACHINED_URL") ?? "http://127.0.0.1:4100";
const api_key = (await Deno.readTextFile(
    Deno.env.get("API_KEY_FILE") ?? `${home}/machine-slop/demo/api-key`,
)).trim();
const copies = Number(Deno.env.get("COPIES") ?? "10");
if (!Number.isInteger(copies) || copies < 1 || copies > 16) {
    throw new Error("COPIES must be an integer between 1 and 16");
}
const maze_seed = Deno.env.get("MAZE_SEED") ?? "75";
const walk_seed = Deno.env.get("WALK_SEED") ?? "1";
const step_seconds = Deno.env.get("STEP_SECONDS") ?? "0.04";
const fork_after_ms = Number(Deno.env.get("FORK_AFTER_MS") ?? "5000");
const linger_ms = Number(Deno.env.get("LINGER_MS") ?? "4000");
const branch_delay_ms = Number(Deno.env.get("BRANCH_DELAY_MS") ?? "2000");
const run_dir = Deno.args[0] ??
    `${home}/machine-slop/demo/runs/race-${
        new Date().toISOString().replace(/[:.]/g, "-")
    }`;

await Deno.mkdir(run_dir, { recursive: true });
const log = await Deno.open(`${run_dir}/events.jsonl`, {
    create: true,
    write: true,
    truncate: true,
});
const started = performance.now();
const encoder = new TextEncoder();
const decoder = new TextDecoder();
function emit(type: string, fields: Record<string, unknown> = {}) {
    const event = {
        t: Math.round(performance.now() - started),
        type,
        ...fields,
    };
    log.writeSync(encoder.encode(JSON.stringify(event) + "\n"));
}

function decode(data: string): string {
    return decoder.decode(Uint8Array.from(atob(data), (c) => c.charCodeAt(0)));
}

type Lane = { id: string; machine: HostedMachine; process: Process };

async function follow(
    lane: Lane,
    from_seq: number,
    signal: AbortSignal,
    until?: (record: Record<string, unknown>) => boolean,
): Promise<number> {
    let next_seq = from_seq;
    let pending = "";
    try {
        for await (const event of lane.process.events(from_seq, signal)) {
            next_seq = event.seq + 1;
            if (event.type !== "stdout") continue;
            pending += decode(event.data);
            const lines = pending.split("\n");
            pending = lines.pop() ?? "";
            for (const line of lines) {
                const record = JSON.parse(line);
                const { t: guest_t, ...fields } = record;
                emit("out", { lane: lane.id, ...fields, guest_t });
                if (until?.(record)) return next_seq;
            }
        }
    } catch (error) {
        if (!signal.aborted) throw error;
        signal.throwIfAborted();
    }
    if (until) {
        throw new Error(`Process ${lane.id} ended before expected output`);
    }
    return next_seq;
}

const host = new Host(machined_url, { apiKey: api_key });
const made: HostedMachine[] = [];
let snapshot_id: string | null = null;
const streams = new AbortController();
const deadline = AbortSignal.any([
    streams.signal,
    AbortSignal.timeout(180_000),
]);
const followers: Promise<unknown>[] = [];

try {
    emit("run", { copies, maze_seed, walk_seed, step_seconds });
    const create_started = performance.now();
    const created = await host.createMachine({
        memory_mib: 128,
        vcpus: 1,
        idle_timeout_secs: 900,
        allowed_hosts: [],
    }, deadline);
    made.push(created.machine);
    emit("machine", {
        lane: "origin",
        id: created.machine.id,
        boot_ms: created.info.boot_ms,
        ms: Math.round(performance.now() - create_started),
    });
    await created.machine.writeFile(
        "/workspace/search.py",
        await Deno.readFile(new URL("../machine/search.py", import.meta.url)),
        deadline,
    );
    const process = await created.machine.startProcess({
        program: "/usr/bin/python3",
        args: ["-u", "search.py", maze_seed, walk_seed, step_seconds],
        cwd: "/workspace",
        keep_stdin_open: true,
    }, deadline);
    const origin: Lane = { id: "origin", machine: created.machine, process };
    emit("process", { lane: "origin", id: process.id });

    const following = follow(
        origin,
        0,
        deadline,
        (record) => record.checkpoint === true,
    );
    followers.push(following);
    following.catch(() => {});
    await delay(fork_after_ms);

    const stdin_offset = await process.writeStdin(0, "freeze\n", deadline);
    const fork_seq = await following;
    const checkpoint = await created.machine.readFile(
        "/workspace/checkpoint.json",
        deadline,
    );
    const checkpoint_text = decoder.decode(checkpoint);
    emit("checkpoint", JSON.parse(checkpoint_text));
    emit("pause_start");
    const pause_started = performance.now();
    await created.machine.pause(deadline);
    emit("paused", { ms: Math.round(performance.now() - pause_started) });

    const snapshot_started = performance.now();
    const snapshot = await created.machine.snapshot(deadline);
    snapshot_id = snapshot.id;
    emit("snapshot", {
        id: snapshot.id,
        stored_bytes: snapshot.stored_bytes,
        ms: Math.round(performance.now() - snapshot_started),
    });

    const fork_started = performance.now();
    const settled = await Promise.allSettled(
        Array.from({ length: copies }, async (_, index) => {
            const lane_id = `copy-${index + 1}`;
            const begun = performance.now();
            const child = await host.createMachine({
                snapshot_id: snapshot.id,
                idle_timeout_secs: 900,
            }, deadline);
            made.push(child.machine);
            const saved = decoder.decode(
                await child.machine.readFile(
                    "/workspace/checkpoint.json",
                    deadline,
                ),
            );
            if (saved !== checkpoint_text) {
                throw new Error("Fork checkpoint differs from origin");
            }
            emit("verified", { lane: lane_id, ...JSON.parse(saved) });
            const ready_ms = Math.round(performance.now() - fork_started);
            emit("copy", {
                lane: lane_id,
                id: child.machine.id,
                boot_ms: child.info.boot_ms,
                ms: Math.round(performance.now() - begun),
                ready_ms,
            });
            return {
                id: lane_id,
                machine: child.machine,
                process: child.machine.process(process.id),
            } satisfies Lane;
        }),
    );
    const lanes = settled.flatMap((result) =>
        result.status === "fulfilled" ? [result.value] : []
    );
    for (const result of settled) {
        if (result.status === "rejected") {
            emit("copy_failed", { error: String(result.reason) });
        }
    }
    emit("forked", {
        copies: lanes.length,
        ms: Math.round(performance.now() - fork_started),
    });

    if (lanes.length !== copies) {
        throw new Error(`Only ${lanes.length}/${copies} copies started`);
    }
    const race = new AbortController();
    const race_signal = AbortSignal.any([race.signal, deadline]);
    const winner = Promise.race(
        lanes.map((lane) => {
            const following = follow(
                lane,
                fork_seq,
                race_signal,
                (record) => record.found === true,
            );
            followers.push(following);
            return following.then(() => lane.id);
        }),
    );
    winner.catch(() => {});
    await delay(branch_delay_ms);
    await Promise.all(
        lanes.map((lane, index) =>
            lane.process.writeStdin(stdin_offset, `${index + 1}\n`, deadline)
                .then(() =>
                    emit("branch", { lane: lane.id, branch: index + 1 })
                )
        ),
    );
    const first = await winner;
    deadline.throwIfAborted();
    emit("winner", { lane: first });
    await delay(linger_ms);
    race.abort();

    const teardown_started = performance.now();
    await Promise.all(
        lanes.map((lane) => lane.machine.destroy().catch(() => {})),
    );
    emit("copies_destroyed", {
        ms: Math.round(performance.now() - teardown_started),
    });

    const resume_started = performance.now();
    await created.machine.resume(deadline);
    emit("resumed", { ms: Math.round(performance.now() - resume_started) });
    await process.writeStdin(stdin_offset, "0\n", deadline);
    await follow(
        origin,
        fork_seq,
        AbortSignal.timeout(120_000),
        (record) => record.found === true,
    );
    emit("usage", { ...(await host.usage()) });
    emit("done");
} catch (error) {
    emit("error", { message: String(error) });
    throw error;
} finally {
    streams.abort();
    await Promise.allSettled(followers);
    await Promise.all(made.map((machine) => machine.destroy().catch(() => {})));
    if (snapshot_id) await host.deleteSnapshot(snapshot_id).catch(() => {});
    log.close();
    console.log(run_dir);
}
