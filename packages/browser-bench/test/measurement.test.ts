import assert from "node:assert/strict";

import type { AttemptRecord } from "../src/attempt.ts";
import { renderAxTree } from "../src/ax.ts";
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
        { pid: 101, ppid: 1, rssBytes: 2048 * 1024, cpuSeconds: 1.5 },
        { pid: 102, ppid: 101, rssBytes: 512 * 1024, cpuSeconds: 3723.25 },
        { pid: 103, ppid: 101, rssBytes: 4096, cpuSeconds: 172_801 },
    ]);
});

Deno.test("linux stat parsing survives spaces and parentheses in the command name", () => {
    const stat = "4242 (Web Content (x) y) S 4000 4242 4242 0 -1 4194560 " +
        "900 0 0 0 150 50 0 0 20 0 12 0 5555 123456789 300 " +
        "18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
    assert.deepEqual(parseLinuxStat(stat, 100, 4096), {
        pid: 4242,
        ppid: 4000,
        rssBytes: 300 * 4096,
        cpuSeconds: 2,
    });
});

Deno.test("usage covers only the browser tree, keeps cpu of exited children and resets step peaks", async () => {
    const mib = 2 ** 20;
    const samples: ProcessRow[][] = [
        [
            { pid: 10, ppid: 1, rssBytes: 100 * mib, cpuSeconds: 1 },
            { pid: 11, ppid: 10, rssBytes: 50 * mib, cpuSeconds: 2 },
            { pid: 12, ppid: 11, rssBytes: 30 * mib, cpuSeconds: 0.5 },
            { pid: 99, ppid: 1, rssBytes: 900 * mib, cpuSeconds: 40 },
        ],
        [
            { pid: 10, ppid: 1, rssBytes: 110 * mib, cpuSeconds: 1.5 },
            { pid: 99, ppid: 1, rssBytes: 900 * mib, cpuSeconds: 41 },
        ],
    ];
    const meter = new UsageMeter(10, () => Promise.resolve(samples.shift()!));

    assert.deepEqual(await meter.mark(), {
        cpuSeconds: 3.5,
        peakRssSumBytes: 180 * mib,
    });
    assert.deepEqual(await meter.mark(), {
        cpuSeconds: 4,
        peakRssSumBytes: 110 * mib,
    });
    assert.equal(meter.peakRssSumBytes(), 180 * mib);
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
    attemptNumber: number,
    failure: AttemptRecord["failure"],
): AttemptRecord {
    return {
        arm: "arm",
        task: "task",
        round,
        attempt: attemptNumber,
        positionInRound: 0,
        status: failure ? "failed" : "passed",
        failure,
        startup: null,
        observerPrepareMs: null,
        steps: [],
        cpuSeconds: 0,
        peakRssSumBytes: 0,
        wallMs: 0,
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
            passedFirstAttempt: 1,
            passedAfterRetries: 2,
            failedAttempts: { timeout: 2, outcome: 1 },
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
