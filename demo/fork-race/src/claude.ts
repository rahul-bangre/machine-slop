export type AgentReply = {
    intent: string;
    commands: string[];
    summary: string | null;
    output_tokens: number;
};

const reply_schema = JSON.stringify({
    type: "object",
    properties: {
        intent: { type: "string" },
        commands: { type: "array", items: { type: "string" } },
        summary: { type: "string" },
    },
    required: ["intent", "commands"],
    additionalProperties: false,
});

type CliResult = {
    is_error: boolean;
    result?: string;
    structured_output?: {
        intent: string;
        commands: string[];
        summary?: string;
    };
    usage?: { output_tokens: number };
};

export class AgentSession {
    private readonly session_id = crypto.randomUUID();
    private turns = 0;

    constructor(
        private readonly model: string,
        private readonly system_prompt: string,
        private readonly workdir: string,
    ) {}

    async send(message: string): Promise<AgentReply> {
        const child = new Deno.Command("claude", {
            args: [
                "--print",
                "--output-format",
                "json",
                "--model",
                this.model,
                "--effort",
                "low",
                "--tools",
                "",
                "--strict-mcp-config",
                "--disable-slash-commands",
                "--setting-sources",
                "",
                "--system-prompt",
                this.system_prompt,
                "--json-schema",
                reply_schema,
                ...(this.turns === 0
                    ? ["--session-id", this.session_id]
                    : ["--resume", this.session_id]),
            ],
            cwd: this.workdir,
            stdin: "piped",
            stdout: "piped",
            stderr: "piped",
        }).spawn();
        const writer = child.stdin.getWriter();
        await writer.write(new TextEncoder().encode(message));
        await writer.close();
        const { stdout, stderr } = await child.output();
        this.turns++;
        const text = new TextDecoder().decode(stdout);
        let result: CliResult;
        try {
            result = JSON.parse(text);
        } catch {
            throw new Error(
                `claude returned no result: ${
                    new TextDecoder().decode(stderr).slice(-400)
                }`,
            );
        }
        if (result.is_error || !result.structured_output) {
            throw new Error(
                `claude failed: ${result.result ?? text.slice(0, 400)}`,
            );
        }
        return {
            intent: result.structured_output.intent,
            commands: result.structured_output.commands,
            summary: result.structured_output.summary ?? null,
            output_tokens: result.usage?.output_tokens ?? 0,
        };
    }
}
