import type { Model, ModelTurn, ToolResult } from "./model.ts";
import { type CommandResult, type Sandbox, SandboxError } from "./sandbox.ts";

export type Limits = {
    max_turns: number;
    command_timeout_ms: number;
    max_tool_output_chars?: number;
};

export type CommandRecord = {
    ms: number;
    exit_code: number | null;
    timed_out: boolean;
    output_chars: number;
};

export type AgentRun = {
    stop: "finished" | "turn_limit" | "refusal" | "max_tokens";
    served_by: string | null;
    turns: number;
    model_ms: number;
    sandbox_ms: number;
    commands: CommandRecord[];
    input_tokens: number;
    cached_input_tokens: number;
    output_tokens: number;
    transcript: unknown;
};

export class ModelError extends Error {
    constructor(message: string, options?: ErrorOptions) {
        super(message, options);
        this.name = "ModelError";
    }
}

const largest_tool_output_chars = 20_000;

export function toolOutput(
    result: CommandResult,
    output_limit = largest_tool_output_chars,
): string {
    const status = result.timed_out
        ? "The command timed out and was killed."
        : `exit code ${result.exit_code}`;
    const output = [
        result.stdout,
        result.stderr && `stderr:\n${result.stderr}`,
    ].filter(Boolean).join("\n");
    if (output.length <= output_limit) {
        return [output, status].filter(Boolean).join("\n");
    }
    const half = Math.floor(output_limit / 2);
    const omitted = output.length - half * 2;
    return [
        output.slice(0, half),
        `[${omitted} characters of output omitted]`,
        output.slice(-half),
        status,
    ].join("\n");
}

export async function runAgent(
    model: Model,
    sandbox: Pick<Sandbox, "run">,
    prompt: string,
    limits: Limits,
): Promise<AgentRun> {
    const conversation = model.begin(prompt);
    const run: AgentRun = {
        stop: "turn_limit",
        served_by: null,
        turns: 0,
        model_ms: 0,
        sandbox_ms: 0,
        commands: [],
        input_tokens: 0,
        cached_input_tokens: 0,
        output_tokens: 0,
        transcript: null,
    };
    let results: ToolResult[] = [];

    while (run.turns < limits.max_turns) {
        const asked_at = performance.now();
        let turn: ModelTurn;
        try {
            turn = await conversation.respond(results);
        } catch (error) {
            if (!model.isOutage(error)) throw error;
            throw new ModelError((error as Error).message, { cause: error });
        }
        run.model_ms += performance.now() - asked_at;
        run.turns++;
        run.transcript = conversation.transcript();
        run.input_tokens += turn.input_tokens;
        run.cached_input_tokens += turn.cached_input_tokens;
        run.output_tokens += turn.output_tokens;
        run.served_by ??= turn.served_by;
        if (turn.served_by !== run.served_by) {
            throw new ModelError(
                `model changed mid-run from ${run.served_by} to ${turn.served_by}`,
            );
        }
        if (turn.stop !== "tool_calls") {
            run.stop = turn.stop;
            return run;
        }

        results = [];
        for (const call of turn.calls) {
            if (call.command === null) {
                results.push({
                    id: call.id,
                    is_error: true,
                    output: "Pass a single command string.",
                });
                continue;
            }
            const started_at = performance.now();
            let result: CommandResult;
            try {
                result = await sandbox.run(
                    call.command,
                    limits.command_timeout_ms,
                );
            } catch (error) {
                throw new SandboxError(
                    `command failed to run: ${
                        error instanceof Error ? error.message : error
                    }`,
                    { cause: error },
                );
            }
            const ms = performance.now() - started_at;
            run.sandbox_ms += ms;
            const output = toolOutput(result, limits.max_tool_output_chars);
            run.commands.push({
                ms,
                exit_code: result.exit_code,
                timed_out: result.timed_out,
                output_chars: output.length,
            });
            results.push({ id: call.id, is_error: false, output });
        }
    }
    return run;
}
