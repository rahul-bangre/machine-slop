import { parseArgs } from "node:util";

import {
    type Arm,
    type AttemptRecord,
    type Conditions,
    runAttempt,
} from "./attempt.ts";
import {
    Browser,
    PinMismatch,
    pinnedArgs,
    pinnedEnvironment,
    viewport,
} from "./browser.ts";
import {
    isPinnedChromiumInstalled,
    pinnedExecutable,
    pinnedProduct,
} from "./chromium.ts";
import { fixtureOrigin, FixtureServer } from "./fixtures/server.ts";
import { observers } from "./observers.ts";
import { readiness } from "./page.ts";
import { armOrder } from "./schedule.ts";
import { renderSummary } from "./summary.ts";
import { tasks as allTasks } from "./tasks.ts";
import { cpuTimeResolutionSeconds } from "./usage.ts";

type Config = {
    browsers: Record<
        string,
        { executable: string; product: string; args: string[] }
    >;
    arms: { name: string; browser: string; observer: string }[];
};

const { values: flags } = parseArgs({
    options: {
        config: {
            type: "string",
            default: new URL("../bench.json", import.meta.url).pathname,
        },
        arms: { type: "string" },
        tasks: { type: "string" },
        live: { type: "boolean", default: false },
        rounds: { type: "string", default: "5" },
        retries: { type: "string", default: "1" },
        "think-ms": { type: "string", default: "500" },
        "step-timeout-ms": { type: "string", default: "20000" },
        out: { type: "string" },
    },
});

function fail(message: string): never {
    console.error(message);
    Deno.exit(2);
}

function selection(flag: string | undefined): string[] | null {
    return flag ? flag.split(",").map((name) => name.trim()) : null;
}

const config: Config = JSON.parse(await Deno.readTextFile(flags.config));

function resolveArm(name: string): Arm {
    const arm = config.arms.find((candidate) => candidate.name === name) ??
        fail(`unknown arm "${name}"`);
    const browser = config.browsers[arm.browser] ??
        fail(`arm "${name}" names unknown browser "${arm.browser}"`);
    if (!observers[arm.observer]) {
        fail(`arm "${name}" names unknown observer "${arm.observer}"`);
    }
    return {
        name,
        observer: arm.observer,
        browser: {
            name: arm.browser,
            executable: browser.executable === "pinned"
                ? pinnedExecutable()
                : browser.executable,
            product: browser.product === "pinned"
                ? pinnedProduct
                : browser.product,
            args: browser.args,
        },
    };
}

const arms = (selection(flags.arms) ?? config.arms.map((arm) => arm.name))
    .map(resolveArm);
const taskSelection = selection(flags.tasks);
const tasks = taskSelection
    ? taskSelection.map((id) =>
        allTasks.find((task) => task.id === id) ?? fail(`unknown task "${id}"`)
    )
    : allTasks.filter((task) =>
        flags.live || task.content === "pinned-fixture"
    );

const usesPinnedChromium = arms.some((arm) =>
    arm.browser.executable === pinnedExecutable()
);
if (usesPinnedChromium && !await isPinnedChromiumInstalled()) {
    fail("pinned Chromium is not installed; run `deno task bench:chromium`");
}

const conditions: Conditions = {
    rounds: Number(flags.rounds),
    retries: Number(flags.retries),
    thinkMs: Number(flags["think-ms"]),
    stepTimeoutMs: Number(flags["step-timeout-ms"]),
    usageSampleMs: 200,
};

const distinctBrowsers = new Map(
    arms.map((arm) => [arm.browser.name, arm.browser]),
);
for (const browser of distinctBrowsers.values()) {
    try {
        await (await Browser.launch(browser)).close();
    } catch (error) {
        if (error instanceof PinMismatch) fail(error.message);
        throw error;
    }
}

const startedAt = new Date();
const outputDirectory = flags.out ??
    `bench-results/${startedAt.toISOString().replaceAll(":", "-")}`;
await Deno.mkdir(outputDirectory, { recursive: true });

await Deno.writeTextFile(
    `${outputDirectory}/run.json`,
    JSON.stringify(
        {
            startedAt: startedAt.toISOString(),
            host: {
                os: Deno.build.os,
                osRelease: Deno.osRelease(),
                arch: Deno.build.arch,
                logicalCpus: navigator.hardwareConcurrency,
                memoryBytes: Deno.systemMemoryInfo().total,
                loadAverageAtStart: Deno.loadavg(),
            },
            conditions: {
                ...conditions,
                viewport,
                pinnedArgs,
                pinnedEnvironment,
                readiness,
                fixtureOrigin,
                freshBrowserAndProfilePerAttempt: true,
                unmeasuredWarmUpLaunchPerBrowser: true,
                armOrder: "rotated by round + task index",
                cpuTimeResolutionSeconds,
                memoryMetric:
                    "peak of summed RSS across the browser process tree; shared pages are counted once per process",
            },
            arms: arms.map((arm) => ({
                ...arm,
                observation: {
                    format: observers[arm.observer]!.format,
                    tokenizer: observers[arm.observer]!.tokenizer,
                },
            })),
            tasks: tasks.map((task) => ({
                id: task.id,
                content: task.content,
                outcomeSource: task.outcome.source,
                steps: task.steps.map((step) => step.name),
            })),
        },
        null,
        2,
    ),
);

const server = new FixtureServer();
const attempts: AttemptRecord[] = [];
const attemptLog = await Deno.open(`${outputDirectory}/attempts.jsonl`, {
    create: true,
    append: true,
});
const encoder = new TextEncoder();

try {
    for (let round = 1; round <= conditions.rounds; round++) {
        for (const [taskIndex, task] of tasks.entries()) {
            const order = armOrder(arms, round, taskIndex);
            for (const [positionInRound, arm] of order.entries()) {
                for (
                    let attempt = 1;
                    attempt <= conditions.retries + 1;
                    attempt++
                ) {
                    const record = await runAttempt({
                        arm,
                        task,
                        round,
                        attempt,
                        positionInRound,
                        conditions,
                        server,
                        outputDirectory,
                    });
                    attempts.push(record);
                    await attemptLog.write(
                        encoder.encode(JSON.stringify(record) + "\n"),
                    );
                    console.error(
                        `round ${round} ${task.id} ${arm.name} attempt ${attempt}: ${record.status}` +
                            (record.failure
                                ? ` (${record.failure.kind}: ${record.failure.message})`
                                : ""),
                    );
                    if (record.status === "passed") break;
                }
            }
        }
    }
} finally {
    attemptLog.close();
    await server.stop();
}

const summary = `# Browser bench ${startedAt.toISOString()}\n\n` +
    renderSummary(
        attempts,
        arms.map((arm) => arm.name),
        tasks.map((task) => task.id),
    );
await Deno.writeTextFile(`${outputDirectory}/summary.md`, summary);
console.log(summary);
console.error(`results written to ${outputDirectory}`);
