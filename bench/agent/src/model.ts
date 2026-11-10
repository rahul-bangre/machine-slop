export type ToolCall = { id: string; command: string | null };

export type ToolResult = { id: string; output: string; is_error: boolean };

export type ModelTurn = {
    stop: "tool_calls" | "finished" | "refusal" | "max_tokens";
    calls: ToolCall[];
    served_by: string;
    input_tokens: number;
    cached_input_tokens: number;
    output_tokens: number;
};

export interface Conversation {
    respond(results: ToolResult[]): Promise<ModelTurn>;
    transcript(): unknown;
}

export interface Model {
    describe(): Record<string, unknown>;
    begin(prompt: string): Conversation;
    isOutage(error: unknown): boolean;
}

export const max_output_tokens = 16_000;

const sandbox_rules = [
    "You are completing a coding task inside a Linux sandbox.",
    "Each command runs in a fresh shell whose working directory is the task directory. Nothing but files persists between commands.",
    "python3 is available. Use only the Python standard library.",
    "Work only inside the task directory.",
];

export const tool_system_prompt = [
    ...sandbox_rules,
    "Run commands with the bash tool.",
    "When the task is complete, reply with a one-sentence summary and no tool call.",
].join("\n");

export const json_system_prompt = [
    ...sandbox_rules,
    "You have no tools. Each reply is a JSON object whose `commands` field lists the shell commands to run next, in order. The next message gives you their output.",
    "When the task is complete, reply with an empty `commands` list and a one-sentence `summary`.",
].join("\n");

export function commandFrom(input: unknown): string | null {
    if (typeof input !== "object" || input === null) return null;
    const { command } = input as { command?: unknown };
    return typeof command === "string" ? command : null;
}
