import Anthropic from "@anthropic-ai/sdk";

export const model_id = "claude-opus-5-5";
export const effort = "medium";
export const max_tokens = 16_000;

export const system_prompt = [
    "You are completing a coding task inside a Linux sandbox.",
    "The bash tool runs each command in a fresh shell whose working directory is the task directory. Nothing but files persists between commands.",
    "python3 is available. Use only the Python standard library.",
    "Work only inside the task directory.",
    "When the task is complete, reply with a one-sentence summary and no tool call.",
].join("\n");

export type Model = {
    describe(): Record<string, unknown>;
    respond(messages: Anthropic.MessageParam[]): Promise<Anthropic.Message>;
};

export function claudeModel(): Model {
    const client = new Anthropic();
    return {
        describe: () => ({
            model: model_id,
            effort,
            max_tokens,
            system_prompt,
            tools: ["bash_20250124"],
            refusal_fallback: "off: a fallback would change the model mid-run",
        }),
        respond: (messages) =>
            client.messages.create({
                model: model_id,
                max_tokens,
                output_config: { effort },
                cache_control: { type: "ephemeral" },
                system: system_prompt,
                tools: [{ type: "bash_20250124", name: "bash" }],
                messages,
            }),
    };
}

export function isModelOutage(error: unknown): boolean {
    return error instanceof Anthropic.APIConnectionError ||
        error instanceof Anthropic.RateLimitError ||
        error instanceof Anthropic.InternalServerError;
}
