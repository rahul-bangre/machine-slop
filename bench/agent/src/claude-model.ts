import Anthropic from "@anthropic-ai/sdk";

import {
    commandFrom,
    type Conversation,
    max_output_tokens,
    type Model,
    type ModelTurn,
    tool_system_prompt,
    type ToolResult,
} from "./model.ts";

export const default_claude_model = "claude-opus-5-5";
const effort = "medium";

export function claudeModel(model_id: string): Model {
    const client = new Anthropic();
    return {
        describe: () => ({
            provider: "anthropic",
            model: model_id,
            effort,
            max_output_tokens,
            system_prompt: tool_system_prompt,
            tool: "bash_20250124",
            refusal_fallback: "off: a fallback would change the model mid-run",
        }),
        isOutage: (error) =>
            error instanceof Anthropic.APIConnectionError ||
            error instanceof Anthropic.RateLimitError ||
            error instanceof Anthropic.InternalServerError,
        begin(prompt): Conversation {
            const messages: Anthropic.MessageParam[] = [
                { role: "user", content: prompt },
            ];
            return {
                transcript: () => messages,
                async respond(results: ToolResult[]): Promise<ModelTurn> {
                    if (results.length > 0) {
                        messages.push({
                            role: "user",
                            content: results.map((result) => ({
                                type: "tool_result",
                                tool_use_id: result.id,
                                content: result.output,
                                is_error: result.is_error,
                            })),
                        });
                    }
                    const response = await client.messages.create({
                        model: model_id,
                        max_tokens: max_output_tokens,
                        output_config: { effort },
                        cache_control: { type: "ephemeral" },
                        system: tool_system_prompt,
                        tools: [{ type: "bash_20250124", name: "bash" }],
                        messages,
                    });
                    messages.push({
                        role: "assistant",
                        content: response.content,
                    });
                    const calls = response.content.filter(
                        (block): block is Anthropic.ToolUseBlock =>
                            block.type === "tool_use",
                    );
                    const cached = response.usage.cache_read_input_tokens ?? 0;
                    return {
                        stop: response.stop_reason === "refusal"
                            ? "refusal"
                            : response.stop_reason === "max_tokens"
                            ? "max_tokens"
                            : calls.length > 0
                            ? "tool_calls"
                            : "finished",
                        calls: calls.map((call) => ({
                            id: call.id,
                            command: commandFrom(call.input),
                        })),
                        served_by: response.model,
                        input_tokens: response.usage.input_tokens + cached +
                            (response.usage.cache_creation_input_tokens ?? 0),
                        cached_input_tokens: cached,
                        output_tokens: response.usage.output_tokens,
                    };
                },
            };
        },
    };
}
