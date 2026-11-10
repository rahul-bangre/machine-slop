import assert from "node:assert/strict";

import type { AttemptRecord } from "../src/attempt.ts";
import { renderAxTree } from "../src/ax.ts";
import { applyDelta, diffLines } from "../src/delta.ts";
import { armOrder } from "../src/schedule.ts";
import { reliability, spread } from "../src/summary.ts";
import {
    parseLinuxStat,
    parsePsRows,
    type ProcessRow,
    UsageMeter,
} from "../src/usage.ts";

Deno.test("ps rows carry rss in bytes and cpu time across minute, hour and day forms", () => {
    const rows = parsePsRows(
        [
            "  101     1  2048   0:01.50",
            "  102   101   512   1:02:03.25",
            "  103   101     4 2-00:00:01",
            "",
        ].join("\n"),
    );
    assert.deepEqual(rows, [
        { pid: 101, ppid: 1, rss_bytes: 2048 * 1024, cpu_seconds: 1.5 },
        { pid: 102, ppid: 101, rss_bytes: 512 * 1024, cpu_seconds: 3723.25 },
        { pid: 103, ppid: 101, rss_bytes: 4096, cpu_seconds: 172_801 },
    ]);
});

Deno.test("linux stat parsing survives spaces and parentheses in the command name", () => {
    const stat = "4242 (Web Content (x) y) S 4000 4242 4242 0 -1 4194560 " +
        "900 0 0 0 150 50 0 0 20 0 12 0 5555 123456789 300 " +
        "18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
    assert.deepEqual(parseLinuxStat(stat, 100, 4096), {
        pid: 4242,
        ppid: 4000,
        rss_bytes: 300 * 4096,
        cpu_seconds: 2,
    });
});

Deno.test("usage covers only the browser tree, keeps cpu of exited children and resets step peaks", async () => {
    const mib = 2 ** 20;
    const samples: ProcessRow[][] = [
        [
            { pid: 10, ppid: 1, rss_bytes: 100 * mib, cpu_seconds: 1 },
            { pid: 11, ppid: 10, rss_bytes: 50 * mib, cpu_seconds: 2 },
            { pid: 12, ppid: 11, rss_bytes: 30 * mib, cpu_seconds: 0.5 },
            { pid: 99, ppid: 1, rss_bytes: 900 * mib, cpu_seconds: 40 },
        ],
        [
            { pid: 10, ppid: 1, rss_bytes: 110 * mib, cpu_seconds: 1.5 },
            { pid: 99, ppid: 1, rss_bytes: 900 * mib, cpu_seconds: 41 },
        ],
    ];
    const meter = new UsageMeter(10, () => Promise.resolve(samples.shift()!));

    assert.deepEqual(await meter.mark(), {
        cpu_seconds: 3.5,
        peak_rss_sum_bytes: 180 * mib,
    });
    assert.deepEqual(await meter.mark(), {
        cpu_seconds: 4,
        peak_rss_sum_bytes: 110 * mib,
    });
    assert.equal(meter.lifetimePeakRssSumBytes(), 180 * mib);
});

Deno.test("every arm runs in every position equally often", () => {
    const arms = ["baseline", "candidate", "other"];
    const positions = new Map(arms.map((arm) => [arm, [0, 0, 0]]));
    for (let round = 1; round <= 6; round++) {
        for (const [position, arm] of armOrder(arms, round, 0).entries()) {
            positions.get(arm)![position]! += 1;
        }
    }
    for (const counts of positions.values()) {
        assert.deepEqual(counts, [2, 2, 2]);
    }
    assert.notDeepEqual(armOrder(arms, 1, 0), armOrder(arms, 1, 1));
});

function attempt(
    round: number,
    attempt_number: number,
    failure: AttemptRecord["failure"],
): AttemptRecord {
    return {
        arm: "arm",
        task: "task",
        round,
        attempt: attempt_number,
        position_in_round: 0,
        status: failure ? "failed" : "passed",
        failure,
        startup: null,
        observer_prepare_ms: null,
        steps: [],
        cpu_seconds: 0,
        peak_rss_sum_bytes: 0,
        wall_ms: 0,
    };
}

Deno.test("reliability keeps failed attempts and separates first-attempt passes from retried ones", () => {
    const timeout = { kind: "timeout" as const, step: "s", message: "" };
    const outcome = { kind: "outcome" as const, step: null, message: "" };
    assert.deepEqual(
        reliability([
            attempt(1, 1, null),
            attempt(2, 1, timeout),
            attempt(2, 2, null),
            attempt(3, 1, timeout),
            attempt(3, 2, outcome),
        ]),
        {
            runs: 3,
            passed_first_attempt: 1,
            passed_after_retries: 2,
            failed_attempts: { timeout: 2, outcome: 1 },
        },
    );
});

Deno.test("spread reports the median and range, and nothing for no passing runs", () => {
    assert.deepEqual(spread([9, 1, 4, 2]), { median: 3, min: 1, max: 9 });
    assert.equal(spread([]), null);
});

Deno.test("accessibility rendering hoists unnamed wrappers and drops text already in its parent's name", () => {
    const text = renderAxTree([
        {
            nodeId: "1",
            role: { value: "RootWebArea" },
            name: { value: "Shop" },
            childIds: ["2"],
        },
        {
            nodeId: "2",
            parentId: "1",
            role: { value: "generic" },
            childIds: ["3", "5"],
        },
        {
            nodeId: "3",
            parentId: "2",
            role: { value: "button" },
            name: { value: "Add to cart" },
            properties: [
                { name: "focused", value: { value: true } },
                { name: "disabled", value: { value: false } },
            ],
            childIds: ["4"],
        },
        {
            nodeId: "4",
            parentId: "3",
            role: { value: "StaticText" },
            name: { value: "Add to cart" },
        },
        {
            nodeId: "5",
            parentId: "2",
            ignored: true,
            childIds: ["6"],
        },
        {
            nodeId: "6",
            parentId: "5",
            role: { value: "StaticText" },
            name: { value: "$40.35" },
        },
    ]);
    assert.equal(
        text,
        [
            `RootWebArea "Shop"`,
            `  button "Add to cart" focused`,
            `  text "$40.35"`,
        ].join("\n"),
    );
});

Deno.test("a line delta carries only the changed lines and rebuilds the next state exactly", () => {
    const previous = [
        `RootWebArea "Shop"`,
        `  status`,
        `    text "48 products"`,
        `  button "Cart (0)"`,
        `  article "Copper Kettle"`,
        `  article "Cobalt Notebook"`,
    ].join("\n");
    const next = [
        `RootWebArea "Shop"`,
        `  status`,
        `    text "Added Cobalt Notebook to cart"`,
        `  button "Cart (1)"`,
        `  article "Cobalt Notebook"`,
        `  dialog "Your cart"`,
    ].join("\n");

    const delta = diffLines(previous, next);
    assert.equal(
        delta,
        [
            `@@ -3,3`,
            `+    text "Added Cobalt Notebook to cart"`,
            `+  button "Cart (1)"`,
            `@@ -7,0`,
            `+  dialog "Your cart"`,
        ].join("\n"),
    );
    assert.equal(applyDelta(previous, delta), next);
    assert.equal(applyDelta(previous, diffLines(previous, previous)), previous);
    assert.equal(applyDelta(previous, diffLines(previous, "")), "");
    assert.equal(applyDelta("", diffLines("", next)), next);
});

Deno.test("a delta that does not fit the previous state is rejected instead of rebuilding a wrong one", () => {
    assert.throws(() => applyDelta("a\nb", "@@ -5,1\n+c"));
    assert.throws(() => applyDelta("a\nb", "@@ -2,4\n+c"));
    assert.throws(() => applyDelta("a\nb", "stray line"));
});
