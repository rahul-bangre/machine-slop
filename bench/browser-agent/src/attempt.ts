import { type AgentRun, type Limits, runAgent } from "../../agent/src/agent.ts";
import type { Model } from "../../agent/src/model.ts";
import type { Sandbox, SandboxProvider } from "../../agent/src/sandbox.ts";
import type { BrowserProfile } from "../../browser/src/browser.ts";
import { type BrowserTask, taskPrompt } from "../runtime/tasks.ts";
import { RemoteRuntime } from "./deploy.ts";

export type Attempt = {
    sandbox: string;
    engine: "native" | "playwright";
    task: string;
    round: number;
    seed: number;
    position: number;
    status: "passed" | "failed";
    failure: string | null;
    create_ms: number;
    reported_boot_ms: number | null;
    setup_ms: number;
    report_ms: number;
    close_ms: number;
    wall_ms: number;
    agent: Omit<AgentRun, "transcript"> | null;
    runtime: unknown;
    cleanup_errors: string[];
};

export async function runBrowserAttempt(context: {
    provider: SandboxProvider;
    engine: Attempt["engine"];
    task: BrowserTask;
    round: number;
    seed: number;
    position: number;
    model: Model;
    limits: Limits;
    browser: BrowserProfile;
    deno: string;
    files: [string, string][];
    artifact: string;
}): Promise<Attempt> {
    const record: Attempt = {
        sandbox: context.provider.name,
        engine: context.engine,
        task: context.task.id,
        round: context.round,
        seed: context.seed,
        position: context.position,
        status: "failed",
        failure: null,
        create_ms: 0,
        reported_boot_ms: null,
        setup_ms: 0,
        report_ms: 0,
        close_ms: 0,
        wall_ms: 0,
        agent: null,
        runtime: null,
        cleanup_errors: [],
    };
    const owned: { sandbox: Sandbox | null; runtime: RemoteRuntime | null } = {
        sandbox: null,
        runtime: null,
    };
    const trace: { transcript: unknown } = { transcript: null };
    const model: Model = {
        ...context.model,
        begin(prompt) {
            const conversation = context.model.begin(prompt);
            return {
                ...conversation,
                async respond(results) {
                    try {
                        return await conversation.respond(results);
                    } finally {
                        trace.transcript = conversation.transcript();
                    }
                },
            };
        },
    };
    const started = performance.now();
    try {
        const create_start = performance.now();
        try {
            owned.sandbox = await context.provider.create();
            record.reported_boot_ms = owned.sandbox.reported_boot_ms;
        } finally {
            record.create_ms = performance.now() - create_start;
        }
        const setup_start = performance.now();
        try {
            owned.runtime = await RemoteRuntime.start(
                owned.sandbox,
                context.files,
                {
                    engine: context.engine,
                    task: context.task.id,
                    seed: context.seed,
                    browser: context.browser,
                },
                context.deno,
            );
        } finally {
            record.setup_ms = performance.now() - setup_start;
        }
        const { transcript, ...agent } = await runAgent(
            model,
            owned.runtime,
            `Goal: ${
                taskPrompt(context.task, context.seed)
            }\nThe browser is at the starting page.`,
            context.limits,
        );
        trace.transcript = transcript;
        record.agent = agent;
    } catch (error) {
        record.failure = String(error);
    } finally {
        if (owned.runtime) {
            const report_start = performance.now();
            try {
                const report = await owned.runtime.report();
                record.runtime = report;
                if (!record.failure) {
                    if (report.checker_failure === null) {
                        record.status = "passed";
                    } else record.failure = String(report.checker_failure);
                }
            } catch (error) {
                record.failure ??= `report: ${String(error)}`;
            } finally {
                record.report_ms = performance.now() - report_start;
            }
        }
        const close_start = performance.now();
        if (owned.runtime) {
            await owned.runtime.close().catch((error) =>
                record.cleanup_errors.push(String(error))
            );
        }
        if (owned.sandbox) {
            await owned.sandbox.close().catch((error) =>
                record.cleanup_errors.push(String(error))
            );
        }
        record.close_ms = performance.now() - close_start;
        record.wall_ms = performance.now() - started;
        await Deno.writeTextFile(
            context.artifact,
            JSON.stringify(trace.transcript, null, 2),
        );
    }
    return record;
}
