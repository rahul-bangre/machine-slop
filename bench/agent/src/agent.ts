import type Anthropic from "@anthropic-ai/sdk";

import { isModelOutage, type Model, model_id } from "./model.ts";
import { type CommandResult, type Sandbox, SandboxError } from "./sandbox.ts";

export type Limits = { max_turns: number; command_timeout_ms: number };

export type CommandRecord = {
    ms: number;
    exit_code: number | null;
    timed_out: boolean;
    output_chars: number;
};

export type AgentStop = "finished" | "turn_limit" | "refusal" | "max_tokens";

export type AgentRun = {
    stop: AgentStop;
    turns: number;
    model_ms: number;
    sandbox_ms: number;
    commands: CommandRecord[];
    input_tokens: number;
    cache_read_input_tokens: number;
    cache_creation_input_tokens: number;
    output_tokens: number;
    transcript: Anthropic.MessageParam[];
};

export class ModelError extends Error {
    constructor(message: string, options?: ErrorOptions) {
        super(message, options);
        this.name = "ModelError";
    }
}

const largest_tool_output_chars = 20_000;

export function toolOutput(result: CommandResult): string {
    const status = result.timed_out
        ? "The command timed out and was killed."
        : `exit code ${result.exit_code}`;
    const output = [
        result.stdout,
        result.stderr && `stderr:\n${result.stderr}`,
    ].filter(Boolean).join("\n");
    if (output.length <= largest_tool_output_chars) {
        return [output, status].filter(Boolean).join("\n");
    }
    const half = largest_tool_output_chars / 2;
    const omitted = output.length - largest_tool_output_chars;
    return [
        output.slice(0, half),
        `[${omitted} characters of output omitted]`,
        output.slice(-half),
        status,
    ].join("\n");
}

function bashCommand(input: unknown): string | null {
    if (typeof input !== "object" || input === null) return null;
    const { command } = input as { command?: unknown };
    return typeof command === "string" ? command : null;
}

export async function runAgent(
    model: Model,
    sandbox: Sandbox,
    prompt: string,
    limits: Limits,
): Promise<AgentRun> {
    const run: AgentRun = {
        stop: "turn_limit",
        turns: 0,
        model_ms: 0,
        sandbox_ms: 0,
        commands: [],
        input_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        output_tokens: 0,
        transcript: [{ role: "user", content: prompt }],
    };

    while (run.turns < limits.max_turns) {
        const asked_at = performance.now();
        let response: Anthropic.Message;
        try {
            response = await model.respond(run.transcript);
        } catch (error) {
            if (!isModelOutage(error)) throw error;
            throw new ModelError((error as Error).message, { cause: error });
        }
        run.model_ms += performance.now() - asked_at;
        run.turns++;
        if (response.model !== model_id) {
            throw new ModelError(
                `response came from ${response.model}, pinned ${model_id}`,
            );
        }
        run.input_tokens += response.usage.input_tokens;
        run.output_tokens += response.usage.output_tokens;
        run.cache_read_input_tokens += response.usage.cache_read_input_tokens ??
            0;
        run.cache_creation_input_tokens +=
            response.usage.cache_creation_input_tokens ?? 0;
        run.transcript.push({ role: "assistant", content: response.content });

        if (response.stop_reason === "refusal") {
            run.stop = "refusal";
            return run;
        }
        if (response.stop_reason === "max_tokens") {
            run.stop = "max_tokens";
            return run;
        }
        const calls = response.content.filter(
            (block): block is Anthropic.ToolUseBlock =>
                block.type === "tool_use",
        );
        if (calls.length === 0) {
            run.stop = "finished";
            return run;
        }

        const results: Anthropic.ToolResultBlockParam[] = [];
        for (const call of calls) {
            const command = bashCommand(call.input);
            if (command === null) {
                results.push({
                    type: "tool_result",
                    tool_use_id: call.id,
                    is_error: true,
                    content:
                        "Each command runs in a fresh shell already; pass a command string.",
                });
                continue;
            }
            const started_at = performance.now();
            let result: CommandResult;
            try {
                result = await sandbox.run(command, limits.command_timeout_ms);
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
            const content = toolOutput(result);
            run.commands.push({
                ms,
                exit_code: result.exit_code,
                timed_out: result.timed_out,
                output_chars: content.length,
            });
            results.push({
                type: "tool_result",
                tool_use_id: call.id,
                content,
            });
        }
        run.transcript.push({ role: "user", content: results });
    }
    return run;
}
