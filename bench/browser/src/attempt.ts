import {
    Browser,
    type BrowserProfile,
    PinMismatch,
    type Startup,
} from "./browser.ts";
import { TimeoutError, withDeadline } from "./cdp.ts";
import type { FixtureServer } from "./fixtures/server.ts";
import { sha256Hex } from "./hex.ts";
import { type Observer, observers } from "./observers.ts";
import { ActionError, Page } from "./page.ts";
import type { Step, Task } from "./tasks.ts";
import { UsageMeter } from "./usage.ts";

export type Arm = { name: string; browser: BrowserProfile; observer: string };

export type Conditions = {
    rounds: number;
    retries: number;
    think_ms: number;
    step_timeout_ms: number;
    usage_sample_ms: number;
};

export type FailureKind =
    | "timeout"
    | "action"
    | "observation"
    | "outcome"
    | "crash";

export type StepRecord = {
    name: string;
    action_ms: number;
    readiness_ms: number;
    observation_ms: number;
    observation_tokens: number;
    observation_bytes: number;
    observation_sha256: string;
    observation_file: string;
    observation_content_checked: boolean;
    active_cpu_seconds: number;
    think_cpu_seconds: number;
    peak_rss_sum_bytes: number;
};

export type AttemptRecord = {
    arm: string;
    task: string;
    round: number;
    attempt: number;
    position_in_round: number;
    status: "passed" | "failed";
    failure: { kind: FailureKind; step: string | null; message: string } | null;
    startup: Startup | null;
    observer_prepare_ms: number | null;
    steps: StepRecord[];
    cpu_seconds: number;
    peak_rss_sum_bytes: number;
    wall_ms: number;
};

const hang_backstop_factor = 1.5;

class ObservationIncomplete extends Error {}
class OutcomeFailed extends Error {}

function failureKind(error: unknown): FailureKind {
    if (error instanceof TimeoutError) return "timeout";
    if (error instanceof ActionError) return "action";
    if (error instanceof ObservationIncomplete) return "observation";
    if (error instanceof OutcomeFailed) return "outcome";
    return "crash";
}

export type AttemptContext = {
    arm: Arm;
    task: Task;
    round: number;
    attempt: number;
    position_in_round: number;
    conditions: Conditions;
    server: FixtureServer;
    output_directory: string;
};

export async function runAttempt(
    context: AttemptContext,
): Promise<AttemptRecord> {
    const { arm, task, conditions, server } = context;
    const observer = observers[arm.observer]!;
    const record: AttemptRecord = {
        arm: arm.name,
        task: task.id,
        round: context.round,
        attempt: context.attempt,
        position_in_round: context.position_in_round,
        status: "failed",
        failure: null,
        startup: null,
        observer_prepare_ms: null,
        steps: [],
        cpu_seconds: 0,
        peak_rss_sum_bytes: 0,
        wall_ms: 0,
    };
    const started_at = performance.now();
    let browser: Browser | null = null;
    let meter: UsageMeter | null = null;
    let current_step: string | null = null;

    try {
        server.startRecording();
        browser = await Browser.launch(arm.browser);
        meter = new UsageMeter(browser.pid);
        meter.start(conditions.usage_sample_ms);
        record.startup = browser.startup;

        const page = await Page.prepare(browser.page);
        const prepare_started_at = performance.now();
        await observer.prepare?.(page);
        record.observer_prepare_ms = performance.now() - prepare_started_at;

        for (const [index, step] of task.steps.entries()) {
            current_step = step.name;
            record.steps.push(
                await runStep(context, observer, page, meter, step, index),
            );
        }
        current_step = null;

        if (task.outcome.source === "server-request-log") {
            const failure = task.outcome.failure(server.recorded());
            if (failure) throw new OutcomeFailed(failure);
        } else if (!await page.evaluate<boolean>(task.outcome.holds)) {
            throw new OutcomeFailed(`page state: ${task.outcome.holds}`);
        }
        record.status = "passed";
    } catch (error) {
        if (error instanceof PinMismatch) throw error;
        record.failure = {
            kind: failureKind(error),
            step: current_step,
            message: error instanceof Error ? error.message : String(error),
        };
    } finally {
        if (meter) {
            await meter.sample();
            meter.stop();
            record.cpu_seconds = meter.totalCpuSeconds();
            record.peak_rss_sum_bytes = meter.lifetimePeakRssSumBytes();
        }
        await browser?.close();
        record.wall_ms = performance.now() - started_at;
    }
    return record;
}

async function runStep(
    context: AttemptContext,
    observer: Observer,
    page: Page,
    meter: UsageMeter,
    step: Step,
    index: number,
): Promise<StepRecord> {
    const { conditions } = context;
    const before = await meter.mark();

    const measured = await withDeadline(
        `step "${step.name}"`,
        conditions.step_timeout_ms * hang_backstop_factor,
        (async () => {
            const action_started_at = performance.now();
            await page.act(step.act);
            const action_ended_at = performance.now();
            const settled_at = await page.settle(
                step.ready,
                conditions.step_timeout_ms,
            );
            const observation_started_at = performance.now();
            const observation = await observer.observe(page);
            return {
                action_ms: action_ended_at - action_started_at,
                readiness_ms: Math.max(0, settled_at - action_ended_at),
                observation_ms: performance.now() - observation_started_at,
                observation,
            };
        })(),
    );
    const after_active = await meter.mark();

    const { observation } = measured;
    const file = [
        "observations",
        context.arm.name,
        context.task.id,
        `r${context.round}-a${context.attempt}-s${index}.${observation.extension}`,
    ].join("/");
    const path = `${context.output_directory}/${file}`;
    await Deno.mkdir(path.slice(0, path.lastIndexOf("/")), { recursive: true });
    await Deno.writeFile(path, observation.bytes);

    if (observation.text !== null) {
        const text = observation.text;
        const missing = (step.observed ?? []).filter((expected) =>
            !text.includes(expected)
        );
        if (missing.length > 0) {
            throw new ObservationIncomplete(
                `observation lacks ${JSON.stringify(missing)} (${file})`,
            );
        }
    }

    await new Promise((resolve) => setTimeout(resolve, conditions.think_ms));
    const after_think = await meter.mark();

    return {
        name: step.name,
        action_ms: measured.action_ms,
        readiness_ms: measured.readiness_ms,
        observation_ms: measured.observation_ms,
        observation_tokens: observation.tokens,
        observation_bytes: observation.bytes.length,
        observation_sha256: await sha256Hex(observation.bytes),
        observation_file: file,
        observation_content_checked: observation.text !== null &&
            (step.observed?.length ?? 0) > 0,
        active_cpu_seconds: after_active.cpu_seconds - before.cpu_seconds,
        think_cpu_seconds: after_think.cpu_seconds - after_active.cpu_seconds,
        peak_rss_sum_bytes: Math.max(
            after_active.peak_rss_sum_bytes,
            after_think.peak_rss_sum_bytes,
        ),
    };
}
