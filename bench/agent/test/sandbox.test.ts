import assert from "node:assert/strict";
import { OpenSandboxes } from "../src/sandbox.ts";

Deno.test("sandbox cleanup shares pending closes and retains failed resources for reclamation", async () => {
    const owned = new OpenSandboxes();
    const first = Promise.withResolvers<void>();
    const state = { closes: 0 };
    const sandbox = owned.track({
        reported_boot_ms: null,
        writeFile: () => Promise.reject(new Error("unexpected write")),
        run: () => Promise.reject(new Error("unexpected command")),
        close() {
            state.closes++;
            return state.closes === 1 ? first.promise : Promise.resolve();
        },
    });
    const closing = assert.rejects(sandbox.close(), /delete unavailable/);
    const reclaiming = assert.rejects(owned.closeAll(), AggregateError);
    first.reject(new Error("delete unavailable"));
    await Promise.all([closing, reclaiming]);
    assert.equal(state.closes, 1);
    await owned.closeAll();
    await Promise.all([sandbox.close(), owned.closeAll()]);
    assert.equal(state.closes, 2);
});
