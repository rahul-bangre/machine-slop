import { type AgentRun, type Limits, ModelError, runAgent } from "./agent.ts";
import type { Model } from "./model.ts";
import { type Sandbox, SandboxError, type SandboxProvider } from "./sandbox.ts";
import type { Task } from "./tasks.ts";

export type FailureKind = "sandbox" | "model" | "turn_limit" | "checker";

export type AttemptRecord = {
    sandbox: string;
    task: string;
    round: number;
    position_in_round: number;
    status: "passed" | "failed";
    failure: { kind: FailureKind; message: string } | null;
    create_ms: number | null;
    setup_ms: number | null;
    agent: Omit<AgentRun, "transcript"> | null;
    check_ms: number | null;
    close_ms: number | null;
    wall_ms: number;
};

export type AttemptContext = {
    provider: SandboxProvider;
    task: Task;
    round: number;
    position_in_round: number;
    model: Model;
    limits: Limits;
    transcript_path: string;
};

class AttemptFailed extends Error {
    constructor(readonly kind: FailureKind, message: string) {
        super(message);
    }
}

const checker_path = ".bench_check.py";
const check_timeout_ms = 60_000;
const longest_checker_message = 2_000;

async function sandboxStep<T>(
    label: string,
    step: () => Promise<T>,
): Promise<T> {
    try {
        return await step();
    } catch (error) {
        throw new AttemptFailed(
            "sandbox",
            `${label}: ${error instanceof Error ? error.message : error}`,
        );
    }
}

async function timed<T>(step: () => Promise<T>): Promise<[T, number]> {
    const started_at = performance.now();
    const value = await step();
    return [value, performance.now() - started_at];
}

export async function runAttempt(
    context: AttemptContext,
): Promise<AttemptRecord> {
    const { provider, task, model, limits } = context;
    const record: AttemptRecord = {
        sandbox: provider.name,
        task: task.id,
        round: context.round,
        position_in_round: context.position_in_round,
        status: "failed",
        failure: null,
        create_ms: null,
        setup_ms: null,
        agent: null,
        check_ms: null,
        close_ms: null,
        wall_ms: 0,
    };
    const started_at = performance.now();
    let sandbox: Sandbox | null = null;

    try {
        [sandbox, record.create_ms] = await timed(() =>
            sandboxStep("create", () => provider.create())
        );
        const opened = sandbox;

        [, record.setup_ms] = await timed(() =>
            sandboxStep("write task files", async () => {
                for (const [path, content] of Object.entries(task.files)) {
                    await opened.writeFile(path, content);
                }
            })
        );

        let run: AgentRun;
        try {
            run = await runAgent(model, opened, task.prompt, limits);
        } catch (error) {
            if (error instanceof ModelError) {
                throw new AttemptFailed("model", error.message);
            }
            if (error instanceof SandboxError) {
                throw new AttemptFailed("sandbox", error.message);
            }
            throw error;
        }
        const { transcript, ...measured } = run;
        record.agent = measured;
        await Deno.writeTextFile(
            context.transcript_path,
            JSON.stringify(transcript, null, 2),
        );
        if (run.stop === "turn_limit") {
            throw new AttemptFailed(
                "turn_limit",
                `no final answer within ${limits.max_turns} turns`,
            );
        }
        if (run.stop !== "finished") {
            throw new AttemptFailed("model", `model stopped with ${run.stop}`);
        }

        const [checked, check_ms] = await timed(() =>
            sandboxStep("run checker", async () => {
                await opened.writeFile(checker_path, task.checker);
                return await opened.run(
                    `python3 ${checker_path}`,
                    check_timeout_ms,
                );
            })
        );
        record.check_ms = check_ms;
        if (checked.exit_code !== 0) {
            const output = `${checked.stdout}${checked.stderr}`.trim();
            throw new AttemptFailed(
                "checker",
                checked.timed_out
                    ? "checker timed out"
                    : output.slice(-longest_checker_message),
            );
        }
        record.status = "passed";
    } catch (error) {
        if (!(error instanceof AttemptFailed)) throw error;
        record.failure = { kind: error.kind, message: error.message };
    } finally {
        if (sandbox) {
            const opened = sandbox;
            const [close_error, close_ms] = await timed(() =>
                opened.close().then(() => null, (error: unknown) => error)
            );
            record.close_ms = close_ms;
            if (close_error !== null && record.failure === null) {
                record.status = "failed";
                record.failure = {
                    kind: "sandbox",
                    message: `close: ${
                        close_error instanceof Error
                            ? close_error.message
                            : close_error
                    }`,
                };
            }
        }
        record.wall_ms = performance.now() - started_at;
    }
    return record;
}
