import assert from "node:assert/strict";
import {
    type DaytonaClient,
    daytonaProvider,
    type DaytonaRemote,
} from "../src/daytona-sandbox.ts";

function fixture(snapshot?: string) {
    const remotes = new Map<string, DaytonaRemote>();
    const deleted: DaytonaRemote[] = [];
    const state = {
        fail_delete: false,
        fail_setup: false,
        lost_create: false,
        hang: false,
        memory: 4,
    };
    const client: DaytonaClient = {
        create(params, options) {
            if (snapshot) {
                assert.ok("snapshot" in params);
                assert.equal(params.snapshot, snapshot);
                assert.ok(!("image" in params));
            } else {
                assert.ok("image" in params);
                assert.deepEqual(params.resources, { cpu: 2, memory: 4 });
            }
            assert.equal(options.timeout, 120);
            assert.equal(params.ttlMinutes, 30);
            const remote: DaytonaRemote = {
                cpu: 2,
                memory: state.memory,
                process: {
                    executeCommand(command, _cwd, _env, timeout) {
                        assert.ok(timeout! > 0);
                        if (state.fail_setup) {
                            return Promise.resolve({
                                exitCode: 1,
                                result: "setup failed",
                            });
                        }
                        if (
                            state.hang && !command.startsWith("mkdir -p /tmp")
                        ) return new Promise(() => {});
                        return Promise.resolve({
                            exitCode: command === "false" ? 1 : 0,
                            result: "output",
                        });
                    },
                },
            };
            remotes.set(params.name!, remote);
            return state.lost_create
                ? Promise.reject(new Error("lost response"))
                : Promise.resolve(remote);
        },
        get(name) {
            return Promise.resolve(remotes.get(name)!);
        },
        delete(remote, timeout, wait) {
            assert.equal(timeout, 30);
            assert.equal(wait, true);
            if (state.fail_delete) {
                return Promise.reject(new Error("delete unavailable"));
            }
            deleted.push(remote);
            return Promise.resolve();
        },
    };
    return {
        provider: daytonaProvider(client, snapshot),
        state,
        remotes,
        deleted,
    };
}

Deno.test("Daytona pins resources, preserves exits, and reclaims only its own sandboxes", async () => {
    for (const source of [undefined, "prepared-browser-snapshot"]) {
        const f = fixture(source);
        const other = fixture();
        await f.provider.prepare();
        const sandbox = await f.provider.create();
        await other.provider.create();
        assert.deepEqual(await sandbox.run("false", 1000), {
            exit_code: 1,
            stdout: "output",
            stderr: "",
            timed_out: false,
        });
        await sandbox.writeFile("nested/with space.txt", "'hello'");
        await assert.rejects(sandbox.writeFile("../escape", "x"));
        await sandbox.close();
        await sandbox.close();
        await f.provider.reclaim();
        assert.equal(f.deleted.length, 1);
        assert.equal(other.deleted.length, 0);
        await other.provider.reclaim();
    }
});

Deno.test("Daytona rejects and destroys a snapshot with different resource limits", async () => {
    const f = fixture("wrong-size-snapshot");
    f.state.memory = 1;
    await assert.rejects(f.provider.create(), /expected 2 \/ 4/);
    assert.equal(f.deleted.length, 1);
});

Deno.test("Daytona cleans up failed setup and a lost create response by owned name", async () => {
    for (const fault of ["fail_setup", "lost_create"] as const) {
        const f = fixture();
        f.state[fault] = true;
        await assert.rejects(f.provider.create());
        assert.equal(f.deleted.length, 1);
    }
});

Deno.test("Daytona timeout destroys before reporting and rejects overlapping or later commands", async () => {
    const f = fixture();
    const sandbox = await f.provider.create();
    f.state.hang = true;
    const running = sandbox.run("sleep 100", 5);
    await assert.rejects(sandbox.run("overlap", 100));
    assert.equal((await running).timed_out, true);
    assert.equal(f.deleted.length, 1);
    await assert.rejects(sandbox.run("later", 100));
});

Deno.test("Daytona failed destruction is a failure, remains tracked, and is retried", async () => {
    const f = fixture();
    const sandbox = await f.provider.create();
    f.state.hang = true;
    f.state.fail_delete = true;
    await assert.rejects(sandbox.run("sleep 100", 5), /delete unavailable/);
    await assert.rejects(sandbox.run("later", 100));
    await assert.rejects(f.provider.reclaim(), AggregateError);
    f.state.fail_delete = false;
    await f.provider.reclaim();
    assert.equal(f.deleted.length, 1);
});

Deno.test("Daytona transport failures remain failures even when cleanup crosses the command deadline", async () => {
    let calls = 0;
    const remote: DaytonaRemote = {
        cpu: 2,
        memory: 4,
        process: {
            executeCommand() {
                return ++calls === 1
                    ? Promise.resolve({ exitCode: 0, result: "" })
                    : Promise.reject(new Error("transport failure"));
            },
        },
    };
    const cleanup = Promise.withResolvers<void>();
    const entered = Promise.withResolvers<void>();
    const provider = daytonaProvider({
        create: () => Promise.resolve(remote),
        get: () => Promise.resolve(remote),
        delete() {
            entered.resolve();
            return cleanup.promise;
        },
    });
    const sandbox = await provider.create();
    const command = sandbox.run("command", 5);
    const rejected = assert.rejects(command, /transport failure/);
    await entered.promise;
    await new Promise((resolve) => setTimeout(resolve, 10));
    cleanup.resolve();
    await rejected;
});

Deno.test("Daytona concurrent close and reclaim share deletion", async () => {
    const f = fixture();
    const sandbox = await f.provider.create();
    await Promise.all([sandbox.close(), sandbox.close(), f.provider.reclaim()]);
    assert.equal(f.deleted.length, 1);
    await assert.rejects(sandbox.run("later", 100));
});

Deno.test("Daytona creation rejects when reclaim wins during setup", async () => {
    const setup = Promise.withResolvers<{ exitCode: number; result: string }>();
    const entered = Promise.withResolvers<void>();
    let deleted = 0;
    const remote: DaytonaRemote = {
        cpu: 2,
        memory: 4,
        process: {
            executeCommand() {
                entered.resolve();
                return setup.promise;
            },
        },
    };
    const provider = daytonaProvider({
        create: () => Promise.resolve(remote),
        get: () => Promise.resolve(remote),
        delete() {
            deleted++;
            return Promise.resolve();
        },
    });
    const creation = provider.create();
    const rejected = assert.rejects(creation, /provider is closed/);
    await entered.promise;
    await provider.reclaim();
    assert.equal(deleted, 1);
    setup.resolve({ exitCode: 0, result: "" });
    await rejected;
    assert.equal(deleted, 1);
});
