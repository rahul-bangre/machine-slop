import assert from "node:assert/strict";
import { agentdProvider } from "../../agent/src/agentd-sandbox.ts";
import {
    pinned_product,
    pinnedExecutable,
} from "../../browser/src/chromium.ts";
import { payload, RemoteRuntime } from "../src/deploy.ts";

Deno.test("deployed runtime survives its launch command and treats model input as literal JSON", async () => {
    const provider = agentdProvider({ binary: "./target/debug/agentd" });
    await provider.prepare();
    const sandbox = await provider.create();
    const directory = await Deno.makeTempDir();
    const marker = `${directory}/injected`;
    const text = `a'b $(touch ${marker}) \`touch ${marker}\``;
    const bundle = await payload();
    try {
        const runtime = await RemoteRuntime.start(sandbox, bundle.files, {
            engine: "native",
            task: "shop-checkout",
            browser: {
                name: "deploy-test",
                executable: pinnedExecutable(),
                product: pinned_product,
                args: ["--no-sandbox"],
            },
        }, Deno.execPath());
        try {
            const result = await runtime.run(
                JSON.stringify({
                    actions: [{
                        kind: "fill",
                        target: { role: "searchbox", name: "Search products" },
                        text,
                    }],
                    wait: {
                        target: { role: "searchbox", name: "Search products" },
                        value: text,
                    },
                }),
                20_000,
            );
            assert.equal(result.exit_code, 0, result.stderr);
            const reply = JSON.parse(result.stdout);
            assert.equal(reply.ok, true, reply.error);
            assert.equal(reply.actions_completed, 1);
            assert.equal(await Deno.stat(marker).catch(() => null), null);
            const report = await runtime.report();
            assert.ok(
                report && typeof report === "object" && "commands" in report,
            );
        } finally {
            await RemoteRuntime.reclaim();
            await runtime.close();
        }
        const cleaned = await sandbox.run("test ! -f control.json", 5_000);
        assert.equal(cleaned.exit_code, 0);
    } finally {
        await sandbox.close();
        await provider.reclaim();
        await Deno.remove(directory, { recursive: true });
    }
});
