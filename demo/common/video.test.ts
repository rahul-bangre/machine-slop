import { strict as assert } from "node:assert";
import { inlineRun } from "./video.ts";

Deno.test("run data injection accepts formatted script indentation and escapes markup", () => {
    const html = inlineRun(
        "<script>\n    const RUN = window.RUN;</script>",
        [{ message: "</script><script>unexpected()</script>" }],
    );
    assert.match(html, /window.RUN = /);
    assert.equal(html.includes("<script>unexpected()"), false);
    const data = html.match(/window.RUN = (.*?);<\/script>/)?.[1];
    assert.ok(data);
    assert.deepEqual(JSON.parse(data), {
        events: [{ message: "</script><script>unexpected()</script>" }],
    });
});

Deno.test("run data injection rejects a dashboard without its entry point", () => {
    assert.throws(() => inlineRun("<html></html>", []), /entry point/);
});
