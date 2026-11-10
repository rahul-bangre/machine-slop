import assert from "node:assert/strict";
import { Browser } from "../../browser/src/browser.ts";
import {
    pinned_product,
    pinnedExecutable,
} from "../../browser/src/chromium.ts";
import type { Driver } from "../runtime/driver.ts";
import { connectNative } from "../runtime/native.ts";

const fixture = `<!doctype html><html><body>
<h1>Native fixture</h1>
<label>Account<input value="original"></label><label>Slow field<input id="slow" value="untouched"></label>
<label><input type="checkbox">Enabled</label>
<button id="open">Open settings</button>
<button disabled>Unavailable</button>
<button id="covered" style="position:absolute;top:600px;left:20px">Covered</button>
<div style="position:absolute;top:590px;left:10px;width:180px;height:70px;background:white;z-index:9"></div>
<section aria-label="West"><button>Pick</button></section>
<section aria-label="East"><button>Pick</button></section>
<button>More options</button><section aria-label="Disclosure panel"><details><summary>More options</summary><button>Nested action</button></details></section>
<table aria-label="Inventory"><thead><tr><th aria-sort="ascending">Product</th><th>Stock</th></tr></thead><tbody>
<tr><td>Alpha</td><td>11</td></tr><tr><td>Beta</td><td>22</td></tr><tr><td>Gamma</td><td>33</td></tr><tr><td>Delta</td><td>44</td></tr>
</tbody></table>
<dialog aria-label="Settings"><div><div><div><label>Deep field<input value="deep"></label><button id="close">Close settings</button></div></div></div></dialog>
<button id="delayed">Delayed action</button><output id="events">0</output><output id="picked"></output><div id="literal">A [literal].*</div><p hidden>Invisible message</p>
<script>
const delayed = document.querySelector('#delayed');
delayed.dataset.presses = '0';
delayed.dataset.clicks = '0';
delayed.onmousedown = () => {
    delayed.dataset.presses = String(Number(delayed.dataset.presses) + 1);
    const until = performance.now() + 160;
    while (performance.now() < until) {}
};
delayed.onclick = () => delayed.dataset.clicks = String(Number(delayed.dataset.clicks) + 1);
document.querySelector('#slow').onclick = () => {
    document.querySelector('#slow').dataset.clicked = 'yes';
    const until = performance.now() + 160;
    while (performance.now() < until) {}
};
const input = document.querySelector('input');
input.addEventListener('input', () => { document.querySelector('#events').textContent = String(Number(document.querySelector('#events').textContent) + 1); });
document.querySelector('#open').onclick = () => document.querySelector('dialog').showModal();
document.querySelector('#close').onclick = () => document.querySelector('dialog').close();
for (const button of document.querySelectorAll('section button')) button.onclick = () => document.querySelector('#picked').textContent = button.parentElement.getAttribute('aria-label');
</script></body></html>`;

function ref(text: string, pattern: RegExp): string {
    const line = text.split("\n").find((line) => pattern.test(line));
    assert.ok(line, `missing control ${pattern}: ${text}`);
    const match = /ref=(\d+\.\d+)/.exec(line);
    assert.ok(match);
    return match[1]!;
}

async function navigate(browser: Browser, driver: Driver): Promise<void> {
    await browser.page.send("Page.navigate", {
        url: `data:text/html,${encodeURIComponent(fixture)}`,
    });
    await driver.wait({ text: "Native fixture" }, 3000);
    await browser.page.send("Runtime.evaluate", {
        expression:
            "new Promise(resolve => { if (document.readyState === 'complete') resolve(); else addEventListener('load', resolve, {once:true}); })",
        awaitPromise: true,
    });
}

Deno.test("native driver delivers fresh scoped accessibility and actual UI input on pinned Chrome", async (test) => {
    const browser = await Browser.launch({
        name: "native-test",
        executable: pinnedExecutable(),
        product: pinned_product,
        args: ["--no-sandbox"],
    });
    const driver = await connectNative(browser);
    try {
        await navigate(browser, driver);
        await test.step("overview identifies controls and bounded table content; full view recovers omissions", async () => {
            const overview = await driver.observe({ mode: "overview" });
            assert.match(overview.scope, /overview/);
            assert.match(overview.text, /heading "Native fixture"/);
            assert.match(overview.text, /textbox "Account".*value="original"/);
            assert.match(overview.text, /checkbox "Enabled".*checked="false"/);
            assert.match(overview.text, /button "Unavailable".*disabled=true/);
            assert.match(
                overview.text,
                /columnheader "Product".*aria-sort="ascending"/,
            );
            for (const name of ["Alpha", "Beta", "Gamma"]) {
                assert.ok(overview.text.includes(name));
            }
            assert.doesNotMatch(overview.text, /cell "Delta"/);
            assert.match(overview.text, /omitted children=/);
            const full = await driver.observe({ mode: "full" });
            assert.equal(full.scope, "document:complete");
            assert.match(full.text, /button "More options".*expanded=false/);
            assert.match(full.text, /cell "Delta"/);
        });
        await test.step("exact scoped targets disambiguate names and parent expansion stays within requested subtree", async () => {
            await assert.rejects(
                driver.act({
                    kind: "click",
                    target: { role: "button", name: "Pick" },
                }, 1000),
                /exactly one/,
            );
            const full = await driver.observe({ mode: "full" });
            const west = ref(full.text, /region "West"/);
            await driver.act({
                kind: "click",
                target: { role: "button", name: "Pick", scope: west },
            }, 1000);
            await driver.wait({ text: "West" }, 1000);
            const scoped = await driver.observe({
                mode: "scope",
                target: { ref: west },
            });
            assert.match(scoped.scope, /subtree:.*complete/);
            assert.match(scoped.text, /button "Pick"/);
            assert.ok(!scoped.text.includes('"East"'));
            const pick = ref(scoped.text, /button "Pick"/);
            const parent = await driver.observe({
                mode: "scope",
                target: { ref: pick },
                parent: 1,
            });
            assert.match(parent.text, /region "West"/);
            const table = await driver.observe({
                mode: "scope",
                target: { role: "table", name: "Inventory" },
            });
            assert.match(table.scope, /subtree:.*complete/);
            assert.match(
                table.text,
                /columnheader "Product".*aria-sort="ascending"/,
            );
            assert.match(table.text, /cell "Delta"/);
            assert.ok(!table.text.includes("Account"));
            assert.ok(!table.text.includes("omitted children"));
            const positions = ["Alpha", "Beta", "Gamma", "Delta"].map((name) =>
                table.text.indexOf(`cell "${name}"`)
            );
            assert.ok(
                positions.every((position, index) =>
                    position >= 0 &&
                    (index === 0 || position > positions[index - 1]!)
                ),
            );

            await assert.rejects(
                driver.observe({
                    mode: "scope",
                    target: { ref: pick },
                    parent: 5,
                }),
                /0 through 4/,
            );
        });
        await test.step("fill replaces text once, fresh observations update within an epoch, and waits read DOM state", async () => {
            const before = await driver.observe({ mode: "full" });
            const account = ref(before.text, /textbox "Account"/);
            await driver.act({
                kind: "fill",
                target: { ref: account },
                text: "updated",
            }, 1000);
            await driver.wait(
                { target: { ref: account }, value: "updated" },
                1000,
            );
            const after = await driver.observe({ mode: "full" });
            assert.match(after.text, /textbox "Account".*value="updated"/);
            assert.equal(ref(after.text, /textbox "Account"/), account);
            const { result } = await browser.page.send<
                { result: { value: string } }
            >("Runtime.evaluate", {
                expression: "document.querySelector('#events').textContent",
                returnByValue: true,
            });
            assert.equal(result.value, "1");
            await driver.act({
                kind: "click",
                target: { role: "checkbox", name: "Enabled" },
            }, 1000);
            await driver.wait({
                target: { role: "checkbox", name: "Enabled" },
                checked: true,
            }, 1000);
            await driver.wait({ text: "A [literal].*" }, 1000);
            await browser.page.send("Runtime.evaluate", {
                expression:
                    "setTimeout(() => { const input = document.createElement('input'); input.setAttribute('aria-label', 'Late arrival'); input.value = 'ready'; document.body.append(input); }, 120)",
            });
            await driver.wait({
                target: { role: "textbox", name: "Late arrival" },
                value: "ready",
            }, 1000);
            await assert.rejects(
                driver.wait({
                    target: { role: "textbox", name: "Never inserted" },
                    value: "ready",
                }, 60),
                /exceeded/,
            );
            await assert.rejects(
                driver.wait({
                    target: { role: "button", name: "Pick" },
                    value: "ready",
                }, 1000),
                /exactly one match; found 2/,
            );

            await assert.rejects(
                driver.wait({ text: "Invisible message" }, 40),
                /exceeded/,
            );
        });
        await test.step("actions reject disabled and obscured controls and expose disclosure through public role", async () => {
            await assert.rejects(
                driver.act({
                    kind: "click",
                    target: { role: "button", name: "Unavailable" },
                }, 1000),
                /disabled/,
            );
            await assert.rejects(
                driver.act({
                    kind: "click",
                    target: { role: "button", name: "Covered" },
                }, 1000),
                /obscured/,
            );
            await assert.rejects(
                driver.act({
                    kind: "click",
                    target: { role: "button", name: "More options" },
                }, 1000),
                /exactly one match; found 2/,
            );
            const full = await driver.observe({ mode: "full" });
            const disclosure = ref(full.text, /region "Disclosure panel"/);
            await driver.act({
                kind: "click",
                target: {
                    role: "button",
                    name: "More options",
                    scope: disclosure,
                },
            }, 1000);
            assert.match(
                (await driver.observe({ mode: "full" })).text,
                /button "More options".*expanded=true/,
            );
        });
        await test.step("overview returns complete active modal including deeply nested controls", async () => {
            await driver.act({
                kind: "click",
                target: { role: "button", name: "Open settings" },
            }, 1000);
            const modal = await driver.observe({ mode: "overview" });
            assert.match(modal.scope, /subtree:.*complete/);
            assert.match(modal.text, /dialog "Settings"/);
            assert.match(modal.text, /textbox "Deep field".*value="deep"/);
            assert.match(modal.text, /button "Close settings"/);
            assert.ok(!modal.text.includes("Native fixture"));
            assert.ok(!modal.text.includes("omitted children"));
            await driver.act({
                kind: "click",
                target: { role: "button", name: "Close settings" },
            }, 1000);
        });
        await test.step("a timed out in-flight input cannot dispatch a later click or replay", async () => {
            const full = await driver.observe({ mode: "full" });
            const delayed = ref(full.text, /button "Delayed action"/);
            await assert.rejects(
                driver.act({ kind: "click", target: { ref: delayed } }, 60),
                /exceeded/,
            );
            const { result } = await browser.page.send<
                { result: { value: { presses: string; clicks: string } } }
            >("Runtime.evaluate", {
                expression:
                    "new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve({...document.querySelector('#delayed').dataset}))))",
                awaitPromise: true,
                returnByValue: true,
            });
            assert.deepEqual(result.value, { presses: "1", clicks: "0" });
            await driver.act({ kind: "click", target: { ref: delayed } }, 1000);
            const completed = await browser.page.send<
                { result: { value: string } }
            >("Runtime.evaluate", {
                expression: "document.querySelector('#delayed').dataset.clicks",
                returnByValue: true,
            });
            assert.equal(completed.result.value, "1");
            const slow = ref(full.text, /textbox "Slow field"/);
            await assert.rejects(
                driver.act({
                    kind: "fill",
                    target: { ref: slow },
                    text: "late",
                }, 60),
                /exceeded/,
            );
            const selected = await browser.page.send<
                {
                    result: {
                        value: {
                            clicked: string;
                            start: number;
                            end: number;
                            value: string;
                        };
                    };
                }
            >("Runtime.evaluate", {
                expression:
                    "new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(() => { const input = document.querySelector('#slow'); resolve({clicked:input.dataset.clicked,start:input.selectionStart,end:input.selectionEnd,value:input.value}); })))",
                awaitPromise: true,
                returnByValue: true,
            });
            assert.equal(selected.result.value.clicked, "yes");
            assert.equal(selected.result.value.value, "untouched");
            assert.equal(
                selected.result.value.start,
                selected.result.value.end,
            );
        });
        await test.step("navigation expires issued references", async () => {
            const before = await driver.observe({ mode: "full" });
            const account = ref(before.text, /textbox "Account"/);
            await navigate(browser, driver);
            await assert.rejects(
                driver.act({
                    kind: "fill",
                    target: { ref: account },
                    text: "bad",
                }, 1000),
                /stale or unobserved/,
            );
            await assert.rejects(
                driver.wait(
                    { target: { ref: account }, value: "original" },
                    1000,
                ),
                /stale or unobserved/,
            );
            assert.match(
                (await driver.observe({ mode: "full" })).text,
                /textbox "Account".*value="original"/,
            );
        });
    } finally {
        await driver.close();
        await browser.close();
    }
});
