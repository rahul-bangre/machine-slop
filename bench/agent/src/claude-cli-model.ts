import {
    type Conversation,
    json_system_prompt,
    type Model,
    type ModelTurn,
    type ToolResult,
} from "./model.ts";

const effort = "medium";

const reply_schema = JSON.stringify({
    type: "object",
    properties: {
        commands: { type: "array", items: { type: "string" } },
        summary: { type: "string" },
    },
    required: ["commands"],
    additionalProperties: false,
});

type CliResult = {
    is_error: boolean;
    api_error_status: number | null;
    stop_reason: string | null;
    result?: string;
    structured_output?: { commands: string[]; summary?: string };
    usage: {
        input_tokens: number;
        output_tokens: number;
        cache_read_input_tokens: number;
        cache_creation_input_tokens: number;
    };
    modelUsage: Record<string, unknown>;
};

class ClaudeCliOutage extends Error {}

function isOutageStatus(status: number | null): boolean {
    return status === 429 || (status !== null && status >= 500);
}

export function claudeCliModel(model_id: string): Model {
    const directories: string[] = [];
    return {
        describe: () => ({
            provider: "claude cli (subscription login)",
            model: model_id,
            effort,
            system_prompt: json_system_prompt,
            tools: "none; commands are returned as schema-validated JSON",
            session: "one headless CLI call per turn, resumed by session id",
            caveat:
                "the CLI adds its own scaffolding tokens and may make small side calls on another model",
        }),
        isOutage: (error) => error instanceof ClaudeCliOutage,
        begin(prompt): Conversation {
            const session_id = crypto.randomUUID();
            const exchanges: { sent: string; reply: unknown }[] = [];
            let commands: string[] = [];

            async function ask(message: string): Promise<CliResult> {
                if (directories.length === 0) {
                    directories.push(
                        await Deno.makeTempDir({ prefix: "agent-bench-cli-" }),
                    );
                }
                const child = new Deno.Command("claude", {
                    args: [
                        "--print",
                        "--output-format",
                        "json",
                        "--model",
                        model_id,
                        "--effort",
                        effort,
                        "--tools",
                        "",
                        "--strict-mcp-config",
                        "--disable-slash-commands",
                        "--setting-sources",
                        "",
                        "--system-prompt",
                        json_system_prompt,
                        "--json-schema",
                        reply_schema,
                        ...(exchanges.length === 0
                            ? ["--session-id", session_id]
                            : ["--resume", session_id]),
                    ],
                    cwd: directories[0]!,
                    stdin: "piped",
                    stdout: "piped",
                    stderr: "piped",
                }).spawn();
                const writer = child.stdin.getWriter();
                await writer.write(new TextEncoder().encode(message));
                await writer.close();
                const { stdout, stderr } = await child.output();
                const decoder = new TextDecoder();
                try {
                    return JSON.parse(decoder.decode(stdout));
                } catch {
                    throw new Error(
                        `claude cli returned no result: ${
                            decoder.decode(stderr).slice(-500)
                        }`,
                    );
                }
            }

            return {
                transcript: () => exchanges,
                async respond(results: ToolResult[]): Promise<ModelTurn> {
                    const sent = results.length === 0
                        ? prompt
                        : results.map((result, index) =>
                            `Output of \`${
                                commands[index]
                            }\`:\n${result.output}`
                        ).join("\n\n");
                    const reply = await ask(sent);
                    exchanges.push({ sent, reply });
                    if (reply.is_error) {
                        const message = `claude cli failed: ${
                            reply.result ?? reply.api_error_status
                        }`;
                        throw isOutageStatus(reply.api_error_status)
                            ? new ClaudeCliOutage(message)
                            : new Error(message);
                    }
                    commands = reply.structured_output?.commands ?? [];
                    return {
                        stop: reply.stop_reason === "refusal"
                            ? "refusal"
                            : reply.stop_reason === "max_tokens"
                            ? "max_tokens"
                            : commands.length > 0
                            ? "tool_calls"
                            : "finished",
                        calls: commands.map((command, index) => ({
                            id: String(index),
                            command,
                        })),
                        served_by: model_id in reply.modelUsage
                            ? model_id
                            : Object.keys(reply.modelUsage).sort().join("+"),
                        input_tokens: reply.usage.input_tokens +
                            reply.usage.cache_read_input_tokens +
                            reply.usage.cache_creation_input_tokens,
                        cached_input_tokens:
                            reply.usage.cache_read_input_tokens,
                        output_tokens: reply.usage.output_tokens,
                    };
                },
            };
        },
    };
}
