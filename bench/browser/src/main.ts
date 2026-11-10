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
    pinned_args,
    pinned_environment,
    viewport,
} from "./browser.ts";
import {
    isPinnedChromiumInstalled,
    pinned_product,
    pinnedExecutable,
} from "./chromium.ts";
import { fixture_origin, FixtureServer } from "./fixtures/server.ts";
import { observers } from "./observers.ts";
import { readiness } from "./page.ts";
import { armOrder } from "./schedule.ts";
import { renderSummary } from "./summary.ts";
import { tasks as all_tasks } from "./tasks.ts";
import { cpu_time_resolution_seconds } from "./usage.ts";

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

function describeObservation(observer_name: string) {
    const observer = observers[observer_name]!();
    return {
        format: observer.format,
        tokenizer: observer.tokenizer,
        delivery: observer.rebuild ? "delta" : "full",
    };
}

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
                ? pinned_product
                : browser.product,
            args: browser.args,
        },
    };
}

const arms = (selection(flags.arms) ?? config.arms.map((arm) => arm.name))
    .map(resolveArm);
const task_selection = selection(flags.tasks);
const tasks = task_selection
    ? task_selection.map((id) =>
        all_tasks.find((task) => task.id === id) ?? fail(`unknown task "${id}"`)
    )
    : all_tasks.filter((task) =>
        flags.live || task.content === "pinned-fixture"
    );

const uses_pinned_chromium = arms.some((arm) =>
    arm.browser.executable === pinnedExecutable()
);
if (uses_pinned_chromium && !await isPinnedChromiumInstalled()) {
    fail("pinned Chromium is not installed; run `deno task bench:chromium`");
}

const conditions: Conditions = {
    rounds: Number(flags.rounds),
    retries: Number(flags.retries),
    think_ms: Number(flags["think-ms"]),
    step_timeout_ms: Number(flags["step-timeout-ms"]),
    usage_sample_ms: 200,
};

const distinct_browsers = new Map(
    arms.map((arm) => [arm.browser.name, arm.browser]),
);
for (const browser of distinct_browsers.values()) {
    try {
        await (await Browser.launch(browser)).close();
    } catch (error) {
        if (error instanceof PinMismatch) fail(error.message);
        throw error;
    }
}

const started_at = new Date();
const output_directory = flags.out ??
    `bench-results/${started_at.toISOString().replaceAll(":", "-")}`;
await Deno.mkdir(output_directory, { recursive: true });

await Deno.writeTextFile(
    `${output_directory}/run.json`,
    JSON.stringify(
        {
            started_at: started_at.toISOString(),
            host: {
                os: Deno.build.os,
                os_release: Deno.osRelease(),
                arch: Deno.build.arch,
                logical_cpus: navigator.hardwareConcurrency,
                memory_bytes: Deno.systemMemoryInfo().total,
                load_average_at_start: Deno.loadavg(),
            },
            conditions: {
                ...conditions,
                viewport,
                pinned_args,
                pinned_environment,
                readiness,
                fixture_origin,
                fresh_browser_and_profile_per_attempt: true,
                unmeasured_warm_up_launch_per_browser: true,
                arm_order: "rotated by round + task index",
                cpu_time_resolution_seconds,
                memory_metric:
                    "peak of summed RSS across the browser process tree; shared pages are counted once per process",
            },
            arms: arms.map((arm) => ({
                ...arm,
                observation: describeObservation(arm.observer),
            })),
            tasks: tasks.map((task) => ({
                id: task.id,
                content: task.content,
                outcome_source: task.outcome.source,
                steps: task.steps.map((step) => step.name),
            })),
        },
        null,
        2,
    ),
);

const server = new FixtureServer();
const attempts: AttemptRecord[] = [];
const attempt_log = await Deno.open(`${output_directory}/attempts.jsonl`, {
    create: true,
    append: true,
});
const encoder = new TextEncoder();

try {
    for (let round = 1; round <= conditions.rounds; round++) {
        for (const [task_index, task] of tasks.entries()) {
            const order = armOrder(arms, round, task_index);
            for (const [position_in_round, arm] of order.entries()) {
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
                        position_in_round,
                        conditions,
                        server,
                        output_directory,
                    });
                    attempts.push(record);
                    await attempt_log.write(
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
    attempt_log.close();
    await server.stop();
}

const summary = `# Browser bench ${started_at.toISOString()}\n\n` +
    renderSummary(
        attempts,
        arms.map((arm) => arm.name),
        tasks.map((task) => task.id),
    );
await Deno.writeTextFile(`${output_directory}/summary.md`, summary);
console.log(summary);
console.error(`results written to ${output_directory}`);
