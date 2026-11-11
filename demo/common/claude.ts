export type AgentReply<T> = { output: T; output_tokens: number };

type CliResult<T> = {
    is_error: boolean;
    result?: string;
    structured_output?: T;
    usage?: { output_tokens: number };
};

export class AgentSession<T> {
    private readonly session_id = crypto.randomUUID();
    private turns = 0;

    constructor(
        private readonly model: string,
        private readonly system_prompt: string,
        private readonly schema: Record<string, unknown>,
        private readonly workdir: string,
        private readonly effort = "low",
    ) {}

    async send(message: string): Promise<AgentReply<T>> {
        const child = new Deno.Command("claude", {
            args: [
                "--print",
                "--output-format",
                "json",
                "--model",
                this.model,
                "--effort",
                this.effort,
                "--tools",
                "",
                "--strict-mcp-config",
                "--disable-slash-commands",
                "--setting-sources",
                "",
                "--system-prompt",
                this.system_prompt,
                "--json-schema",
                JSON.stringify(this.schema),
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
        let result: CliResult<T>;
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
            output: result.structured_output,
            output_tokens: result.usage?.output_tokens ?? 0,
        };
    }
}
