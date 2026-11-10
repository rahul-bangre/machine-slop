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
    thinkMs: number;
    stepTimeoutMs: number;
    usageSampleMs: number;
};

export type FailureKind =
    | "timeout"
    | "action"
    | "observation"
    | "outcome"
    | "crash";

export type StepRecord = {
    name: string;
    actionMs: number;
    readinessMs: number;
    observationMs: number;
    observationTokens: number;
    observationBytes: number;
    observationSha256: string;
    observationFile: string;
    observationContentChecked: boolean;
    activeCpuSeconds: number;
    thinkCpuSeconds: number;
    peakRssSumBytes: number;
};

export type AttemptRecord = {
    arm: string;
    task: string;
    round: number;
    attempt: number;
    positionInRound: number;
    status: "passed" | "failed";
    failure: { kind: FailureKind; step: string | null; message: string } | null;
    startup: Startup | null;
    observerPrepareMs: number | null;
    steps: StepRecord[];
    cpuSeconds: number;
    peakRssSumBytes: number;
    wallMs: number;
};

const hangBackstopFactor = 1.5;

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
    positionInRound: number;
    conditions: Conditions;
    server: FixtureServer;
    outputDirectory: string;
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
        positionInRound: context.positionInRound,
        status: "failed",
        failure: null,
        startup: null,
        observerPrepareMs: null,
        steps: [],
        cpuSeconds: 0,
        peakRssSumBytes: 0,
        wallMs: 0,
    };
    const startedAt = performance.now();
    let browser: Browser | null = null;
    let meter: UsageMeter | null = null;
    let currentStep: string | null = null;

    try {
        server.startRecording();
        browser = await Browser.launch(arm.browser);
        meter = new UsageMeter(browser.pid);
        meter.start(conditions.usageSampleMs);
        record.startup = browser.startup;

        const page = await Page.prepare(browser.page);
        const prepareStartedAt = performance.now();
        await observer.prepare?.(page);
        record.observerPrepareMs = performance.now() - prepareStartedAt;

        for (const [index, step] of task.steps.entries()) {
            currentStep = step.name;
            record.steps.push(
                await runStep(context, observer, page, meter, step, index),
            );
        }
        currentStep = null;

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
            step: currentStep,
            message: error instanceof Error ? error.message : String(error),
        };
    } finally {
        if (meter) {
            await meter.sample();
            meter.stop();
            record.cpuSeconds = meter.cpuSeconds();
            record.peakRssSumBytes = meter.peakRssSumBytes();
        }
        await browser?.close();
        record.wallMs = performance.now() - startedAt;
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
        conditions.stepTimeoutMs * hangBackstopFactor,
        (async () => {
            const actionStartedAt = performance.now();
            await page.act(step.act);
            const actionEndedAt = performance.now();
            const settledAt = await page.settle(
                step.ready,
                conditions.stepTimeoutMs,
            );
            const observationStartedAt = performance.now();
            const observation = await observer.observe(page);
            return {
                actionMs: actionEndedAt - actionStartedAt,
                readinessMs: Math.max(0, settledAt - actionEndedAt),
                observationMs: performance.now() - observationStartedAt,
                observation,
            };
        })(),
    );
    const afterActive = await meter.mark();

    const { observation } = measured;
    const file = [
        "observations",
        context.arm.name,
        context.task.id,
        `r${context.round}-a${context.attempt}-s${index}.${observation.extension}`,
    ].join("/");
    const path = `${context.outputDirectory}/${file}`;
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

    await new Promise((resolve) => setTimeout(resolve, conditions.thinkMs));
    const afterThink = await meter.mark();

    return {
        name: step.name,
        actionMs: measured.actionMs,
        readinessMs: measured.readinessMs,
        observationMs: measured.observationMs,
        observationTokens: observation.tokens,
        observationBytes: observation.bytes.length,
        observationSha256: await sha256Hex(observation.bytes),
        observationFile: file,
        observationContentChecked: observation.text !== null &&
            (step.observed?.length ?? 0) > 0,
        activeCpuSeconds: afterActive.cpuSeconds - before.cpuSeconds,
        thinkCpuSeconds: afterThink.cpuSeconds - afterActive.cpuSeconds,
        peakRssSumBytes: Math.max(
            afterActive.peakRssSumBytes,
            afterThink.peakRssSumBytes,
        ),
    };
}
