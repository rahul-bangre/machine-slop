import {
    Browser,
    BrowserExited,
    type BrowserProfile,
    type Startup,
} from "./browser.ts";
import { CdpError, TimeoutError, withDeadline } from "./cdp.ts";
import type { FixtureServer } from "./fixtures/server.ts";
import { sha256Hex } from "./hex.ts";
import type { Observation, Observer } from "./observers.ts";
import { ActionError, Page } from "./page.ts";
import type { Step, Task } from "./tasks.ts";
import { UsageMeter } from "./usage.ts";

export type Arm = {
    name: string;
    browser: BrowserProfile;
    observer_name: string;
    observer: Observer;
};

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
    readiness_confirmation_ms: number;
    observation_ms: number;
    observation_tokens: number;
    observation_bytes: number;
    observation_sha256: string;
    observation_file: string;
    observation_delivery: Observation["delivery"];
    observation_content_checked: boolean;
    active_cpu_seconds: number;
    think_cpu_seconds: number;
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

export class ObservationRejected extends Error {}
class OutcomeFailed extends Error {}

function browserFailureKind(error: unknown): FailureKind | null {
    if (error instanceof TimeoutError) return "timeout";
    if (error instanceof ActionError) return "action";
    if (error instanceof ObservationRejected) return "observation";
    if (error instanceof OutcomeFailed) return "outcome";
    if (error instanceof CdpError || error instanceof BrowserExited) {
        return "crash";
    }
    return null;
}

export function observedState(
    observer: Observer,
    previous_state: string | null,
    observation: { delivery: Observation["delivery"]; text: string },
    expected: string[],
): string {
    let state = observation.text;
    if (observation.delivery === "delta") {
        if (!observer.rebuild || previous_state === null) {
            throw new ObservationRejected(
                "delta sent with no previous state to apply it to",
            );
        }
        try {
            state = observer.rebuild(previous_state, observation.text);
        } catch (error) {
            throw new ObservationRejected(
                `delta does not apply to the previous state: ${
                    error instanceof Error ? error.message : error
                }`,
            );
        }
    }
    const missing = expected.filter((text) => !state.includes(text));
    if (missing.length > 0) {
        throw new ObservationRejected(
            `observed state lacks ${JSON.stringify(missing)}`,
        );
    }
    return state;
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
    let state: string | null = null;

    try {
        server.startRecording();
        browser = await Browser.launch(arm.browser);
        meter = new UsageMeter(browser.pid);
        meter.start(conditions.usage_sample_ms);
        record.startup = browser.startup;

        const page = await Page.prepare(browser.page);
        const prepare_started_at = performance.now();
        await arm.observer.prepare?.(page);
        record.observer_prepare_ms = performance.now() - prepare_started_at;

        for (const [index, step] of task.steps.entries()) {
            current_step = step.name;
            const observed = await runStep(
                context,
                state,
                page,
                meter,
                step,
                index,
            );
            state = observed.state;
            record.steps.push(observed.record);
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
        const kind = browserFailureKind(error);
        if (!kind) throw error;
        record.failure = {
            kind,
            step: current_step,
            message: (error as Error).message,
        };
    } finally {
        if (meter) {
            record.cpu_seconds = await meter.cpuSecondsNow();
            record.peak_rss_sum_bytes = meter.peakRssSumBytes();
            meter.stop();
        }
        await browser?.close();
        record.wall_ms = performance.now() - started_at;
    }
    return record;
}

async function runStep(
    context: AttemptContext,
    previous_state: string | null,
    page: Page,
    meter: UsageMeter,
    step: Step,
    index: number,
): Promise<{ record: StepRecord; state: string | null }> {
    const { conditions } = context;
    const { observer } = context.arm;
    const cpu_before = await meter.cpuSecondsNow();

    const measured = await withDeadline(
        `step "${step.name}"`,
        conditions.step_timeout_ms * hang_backstop_factor,
        (async () => {
            const action_started_at = performance.now();
            await page.act(step.act);
            const action_ended_at = performance.now();
            const settled = await page.settle(
                step.ready,
                conditions.step_timeout_ms,
            );
            const observation_started_at = performance.now();
            const observation = await observer.observe(page, previous_state);
            const readiness_ms = Math.max(
                0,
                settled.ready_at - action_ended_at,
            );
            return {
                action_ms: action_ended_at - action_started_at,
                readiness_ms,
                readiness_confirmation_ms: settled.confirmed_at -
                    action_ended_at - readiness_ms,
                observation_ms: performance.now() - observation_started_at,
                observation,
            };
        })(),
    );
    const cpu_after_active = await meter.cpuSecondsNow();

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

    const expected = step.observed ?? [];
    let state: string | null = null;
    if (observation.text !== null) {
        state = observedState(
            observer,
            previous_state,
            { delivery: observation.delivery, text: observation.text },
            expected,
        );
        if (observation.delivery === "delta") {
            await Deno.writeTextFile(`${path}.rebuilt-state.txt`, state);
        }
    }

    await new Promise((resolve) => setTimeout(resolve, conditions.think_ms));
    const cpu_after_think = await meter.cpuSecondsNow();

    return {
        state,
        record: {
            name: step.name,
            action_ms: measured.action_ms,
            readiness_ms: measured.readiness_ms,
            readiness_confirmation_ms: measured.readiness_confirmation_ms,
            observation_ms: measured.observation_ms,
            observation_tokens: observation.tokens,
            observation_bytes: observation.bytes.length,
            observation_sha256: await sha256Hex(observation.bytes),
            observation_file: file,
            observation_delivery: observation.delivery,
            observation_content_checked: state !== null && expected.length > 0,
            active_cpu_seconds: cpu_after_active - cpu_before,
            think_cpu_seconds: cpu_after_think - cpu_after_active,
        },
    };
}
