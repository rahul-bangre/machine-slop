import assert from "node:assert/strict";
import { Browser } from "../../browser/src/browser.ts";
import {
    pinned_product,
    pinnedExecutable,
} from "../../browser/src/chromium.ts";
import { connectPlaywright } from "../runtime/playwright.ts";
import type { Driver } from "../runtime/driver.ts";

async function fixture(
    html: string,
): Promise<{ browser: Browser; driver: Driver }> {
    const browser = await Browser.launch({
        name: "playwright-test",
        executable: pinnedExecutable(),
        product: pinned_product,
        args: ["--no-sandbox"],
    });
    try {
        await browser.page.send("Page.navigate", {
            url: `data:text/html,${encodeURIComponent(html)}`,
        });
        const driver = await connectPlaywright(browser.endpoint);
        await driver.wait({ text: "Fixture ready" }, 5_000);
        return { browser, driver };
    } catch (error) {
        await browser.close();
        throw error;
    }
}

function refFor(text: string, role: string, name: string): string {
    const line = text.split("\n").find((line) =>
        line.includes(`${role} "${name}"`)
    );
    const ref = line?.match(/\[ref=([^\]]+)\]/)?.[1];
    assert.ok(ref, `missing ref for ${role} ${name}: ${text}`);
    return ref;
}

Deno.test("Playwright captures canonical visible state and requested subtrees with full fallback", async () => {
    const { browser, driver } = await fixture(`
        <h1>Fixture ready</h1>
        <section aria-label="Preferences">
            <h2>Delivery choices</h2>
            <label><input type="checkbox" checked>Express delivery</label>
            <button disabled>Unavailable</button>
            <button aria-expanded="false">Details</button>
            <button hidden>Hidden secret</button>
        </section>
        <h2>Other section</h2>
        <ul>${
        Array.from({ length: 220 }, (_, index) => `<li>Item ${index}</li>`)
            .join("")
    }</ul>
    `);
    try {
        const overview = await driver.observe({ mode: "overview" });
        assert.match(overview.text, /omitted/);
        assert.doesNotMatch(overview.text, /Item 219/);
        const full = await driver.observe({ mode: "full" });
        assert.match(full.text, /Item 219/);
        assert.match(full.text, /checkbox "Express delivery".*\[checked\]/);
        assert.match(full.text, /button "Unavailable".*\[disabled\]/);
        assert.doesNotMatch(full.text, /Hidden secret/);
        const scope = await driver.observe({
            mode: "scope",
            target: { ref: refFor(full.text, "heading", "Delivery choices") },
            parent: 1,
        });
        assert.match(scope.text, /Express delivery/);
        assert.doesNotMatch(scope.text, /Other section|Item 219/);
        await assert.rejects(
            driver.act({
                kind: "click",
                target: {
                    role: "checkbox",
                    name: "Express delivery",
                    scope: "",
                },
            }, 1_000),
            /unknown or stale reference/,
        );
        await driver.act({
            kind: "click",
            target: {
                role: "checkbox",
                name: "Express delivery",
                scope: refFor(full.text, "region", "Preferences"),
            },
        }, 1_000);
        await driver.wait({
            target: { role: "checkbox", name: "Express delivery" },
            checked: false,
        }, 1_000);
    } finally {
        await driver.close();
        await browser.close();
    }
});

Deno.test("Playwright actions compose without observations and overview immediately focuses a modal", async () => {
    const { browser, driver } = await fixture(`
        <h1>Fixture ready</h1><p>Background content</p>
        <label>Project name<input oninput="document.querySelector('button').disabled=false"></label>
        <button disabled onclick="document.querySelector('dialog').showModal()">Create</button>
        <button onclick="document.body.dataset.wrong='yes'">Create another</button>
        <dialog aria-label="Confirm project">
            <ul>${
        Array.from(
            { length: 200 },
            (_, index) => `<li>Review item ${index}</li>`,
        ).join("")
    }</ul>
            ${
        Array.from({ length: 8 }, (_, index) =>
            `<div role="group" aria-label="Confirmation group ${index}">`).join(
                "",
            )
    }
            <label>Confirmation<input></label>
            ${"</div>".repeat(8)}
            <button onclick="this.closest('dialog').close();document.querySelector('output').textContent='Saved'">Save</button>
        </dialog><output></output>
    `);
    try {
        const initial = await driver.observe({ mode: "full" });
        await driver.act({
            kind: "fill",
            target: { ref: refFor(initial.text, "textbox", "Project name") },
            text: "Sample",
        }, 1_000);
        await driver.act({
            kind: "click",
            target: { role: "button", name: "Create" },
        }, 1_000);
        const modal = await driver.observe({ mode: "overview" });
        assert.equal(modal.scope, "active modal dialog");
        assert.match(modal.text, /dialog "Confirm project"/);
        assert.doesNotMatch(modal.text, /Background content|omitted/);
        assert.match(modal.text, /Review item 199/);
        assert.match(modal.text, /textbox "Confirmation"/);
        await driver.act({
            kind: "fill",
            target: { ref: refFor(modal.text, "textbox", "Confirmation") },
            text: "Accepted",
        }, 1_000);
        await driver.wait({
            target: { role: "textbox", name: "Confirmation" },
            value: "Accepted",
        }, 1_000);
        await driver.act({
            kind: "click",
            target: { ref: refFor(modal.text, "button", "Save") },
        }, 1_000);
        await driver.wait({ text: "Saved" }, 1_000);
        await driver.close();
        const result = await browser.page.send<{ result: { value: string } }>(
            "Runtime.evaluate",
            { expression: "document.querySelector('output').textContent" },
        );
        assert.equal(result.result.value, "Saved");
    } finally {
        await driver.close();
        await browser.close();
    }
});

Deno.test("Playwright rejects previous document refs even when the new document reuses native ref numbers", async () => {
    const { browser, driver } = await fixture(
        "<h1>Fixture ready</h1><button>Same button</button>",
    );
    try {
        const before = await driver.observe({ mode: "full" });
        const old_ref = refFor(before.text, "button", "Same button");
        await browser.page.send("Page.navigate", {
            url: `data:text/html,${
                encodeURIComponent(
                    "<h1>New document</h1><button>Same button</button>",
                )
            }`,
        });
        await driver.wait({ text: "New document" }, 5_000);
        const after = await driver.observe({ mode: "full" });
        assert.notEqual(refFor(after.text, "button", "Same button"), old_ref);
        await assert.rejects(
            driver.act({ kind: "click", target: { ref: old_ref } }, 500),
            /stale reference/,
        );
        await driver.act({
            kind: "click",
            target: { ref: refFor(after.text, "button", "Same button") },
        }, 500);
    } finally {
        await driver.close();
        await browser.close();
    }
});
