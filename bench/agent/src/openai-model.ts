import OpenAI from "openai";

import {
    commandFrom,
    type Conversation,
    max_output_tokens,
    type Model,
    type ModelTurn,
    tool_system_prompt,
    type ToolResult,
} from "./model.ts";

const bash_tool: OpenAI.Chat.Completions.ChatCompletionFunctionTool = {
    type: "function",
    function: {
        name: "bash",
        description: "Run a shell command in the task directory.",
        strict: true,
        parameters: {
            type: "object",
            properties: { command: { type: "string" } },
            required: ["command"],
            additionalProperties: false,
        },
    },
};

function parsedArguments(json: string): unknown {
    try {
        return JSON.parse(json);
    } catch {
        return null;
    }
}

export function openaiModel(model_id: string): Model {
    const client = new OpenAI();
    return {
        describe: () => ({
            provider: "openai",
            model: model_id,
            api: "chat completions",
            reasoning_effort: "provider default",
            max_output_tokens,
            system_prompt: tool_system_prompt,
            tool: bash_tool.function,
        }),
        isOutage: (error) =>
            error instanceof OpenAI.APIConnectionError ||
            error instanceof OpenAI.RateLimitError ||
            error instanceof OpenAI.InternalServerError,
        begin(prompt): Conversation {
            const messages:
                OpenAI.Chat.Completions.ChatCompletionMessageParam[] = [
                    { role: "system", content: tool_system_prompt },
                    { role: "user", content: prompt },
                ];
            return {
                transcript: () => messages,
                async respond(results: ToolResult[]): Promise<ModelTurn> {
                    for (const result of results) {
                        messages.push({
                            role: "tool",
                            tool_call_id: result.id,
                            content: result.output,
                        });
                    }
                    const response = await client.chat.completions.create({
                        model: model_id,
                        max_completion_tokens: max_output_tokens,
                        tools: [bash_tool],
                        messages,
                    });
                    const choice = response.choices[0];
                    if (!choice) throw new Error("response had no choices");
                    messages.push(choice.message);
                    const calls = (choice.message.tool_calls ?? []).filter(
                        (call) => call.type === "function",
                    );
                    return {
                        stop: choice.message.refusal ||
                                choice.finish_reason === "content_filter"
                            ? "refusal"
                            : choice.finish_reason === "length"
                            ? "max_tokens"
                            : calls.length > 0
                            ? "tool_calls"
                            : "finished",
                        calls: calls.map((call) => ({
                            id: call.id,
                            command: commandFrom(
                                parsedArguments(call.function.arguments),
                            ),
                        })),
                        served_by: response.model,
                        input_tokens: response.usage?.prompt_tokens ?? 0,
                        cached_input_tokens: response.usage
                            ?.prompt_tokens_details?.cached_tokens ?? 0,
                        output_tokens: response.usage?.completion_tokens ?? 0,
                    };
                },
            };
        },
    };
}
