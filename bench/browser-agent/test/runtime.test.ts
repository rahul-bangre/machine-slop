import assert from "node:assert/strict";
import {
    pinned_product,
    pinnedExecutable,
} from "../../browser/src/chromium.ts";
import type { CommandRecord, Reply } from "../runtime/session.ts";
import { serve } from "../runtime/worker.ts";

Deno.test("browser runtime preserves partial actions, returns recovery state, and grades server outcomes", async (test) => {
    for (const engine of ["native", "playwright"] as const) {
        await test.step(engine, async () => {
            const directory = await Deno.makeTempDir();
            const control_path = `${directory}/control.json`;
            const running = serve({
                engine,
                task: "shop-checkout",
                browser: {
                    name: "runtime-test",
                    executable: pinnedExecutable(),
                    product: pinned_product,
                    args: ["--no-sandbox"],
                },
            }, control_path);
            running.catch(() => {});
            const control: { port: number; token: string } = await Promise.race(
                [
                    running.then(() => {
                        throw new Error("runtime exited during startup");
                    }),
                    readControl(control_path),
                ],
            );
            const request = async (operation: string, body: unknown = {}) => {
                const response = await fetch(
                    `http://127.0.0.1:${control.port}/${operation}`,
                    {
                        method: "POST",
                        headers: { authorization: `Bearer ${control.token}` },
                        body: JSON.stringify(body),
                        signal: AbortSignal.timeout(20_000),
                    },
                );
                assert.equal(response.status, 200);
                return response.json();
            };
            const command = async (body: unknown): Promise<Reply> =>
                await request("command", body);
            try {
                const unauthorized = await fetch(
                    `http://127.0.0.1:${control.port}/command`,
                    { method: "POST" },
                );
                assert.equal(unauthorized.status, 401);
                await unauthorized.body?.cancel();
                const failed = await command({
                    actions: [
                        {
                            kind: "fill",
                            target: {
                                role: "searchbox",
                                name: "Search products",
                            },
                            text: "cobalt notebook",
                        },
                        {
                            kind: "click",
                            target: { role: "button", name: "Missing button" },
                        },
                        {
                            kind: "click",
                            target: { role: "button", name: "Cart (0)" },
                        },
                    ],
                    timeout_ms: 300,
                });
                assert.equal(failed.ok, false);
                assert.equal(failed.actions_completed, 1);
                assert.ok(failed.error);
                assert.ok(failed.state);
                assert.doesNotMatch(failed.state.scope, /modal/);
                const filtered = await command({
                    wait: { text: "1 products" },
                });
                assert.equal(filtered.ok, true, filtered.error ?? "");
                const cart = await command({
                    actions: [
                        {
                            kind: "click",
                            target: { role: "button", name: "Add to cart" },
                        },
                        {
                            kind: "click",
                            target: { role: "button", name: "Cart (1)" },
                        },
                        {
                            kind: "click",
                            target: { role: "button", name: "Checkout" },
                        },
                    ],
                });
                assert.equal(cart.ok, true, cart.error ?? "");
                assert.equal(cart.actions_completed, 3);
                assert.match(cart.state!.text, /Full name/);
                const ordered = await command({
                    actions: [
                        {
                            kind: "fill",
                            target: { role: "textbox", name: "Full name" },
                            text: "Ada Lovelace",
                        },
                        {
                            kind: "fill",
                            target: { role: "textbox", name: "Email" },
                            text: "ada@example.com",
                        },
                        {
                            kind: "click",
                            target: {
                                role: "checkbox",
                                name: "I accept the terms",
                            },
                        },
                        {
                            kind: "click",
                            target: { role: "button", name: "Place order" },
                        },
                    ],
                    wait: { text: "Order ORD-1001 confirmed" },
                });
                assert.equal(ordered.ok, true, ordered.error ?? "");
                const report: {
                    checker_failure: string | null;
                    commands: CommandRecord[];
                } = await request("report");
                assert.equal(report.checker_failure, null);
                assert.equal(report.commands.length, 4);
                assert.ok(report.commands[0]!.action_ms > 0);
                assert.ok(
                    report.commands.every((record) =>
                        record.reply_tokens > 0 &&
                        record.wall_ms >= record.observation_ms
                    ),
                );
                await command({
                    actions: [{
                        kind: "click",
                        target: { role: "button", name: "Place order" },
                    }],
                    wait: { text: "Order ORD-1002 confirmed" },
                });
                const duplicate: { checker_failure: string | null } =
                    await request("report");
                assert.match(
                    duplicate.checker_failure!,
                    /expected one POST.*saw 2/,
                );
            } finally {
                await request("stop").catch(() => {});
                await running;
                assert.equal(
                    await Deno.stat(control_path).catch(() => null),
                    null,
                );
                await Deno.remove(directory, { recursive: true });
            }
        });
    }
});

async function readControl(
    path: string,
): Promise<{ port: number; token: string }> {
    const deadline = performance.now() + 15_000;
    while (performance.now() < deadline) {
        const text = await Deno.readTextFile(path).catch((error) => {
            if (!(error instanceof Deno.errors.NotFound)) throw error;
            return null;
        });
        if (text) return JSON.parse(text);
        await new Promise((resolve) => setTimeout(resolve, 20));
    }
    throw new Error("runtime did not become ready");
}
