import { encode } from "gpt-tokenizer/encoding/o200k_base";
import { TimeoutError, withDeadline } from "../../browser/src/cdp.ts";
import type { Driver, State } from "./driver.ts";
import { parseCommand } from "./protocol.ts";

export type Reply = {
    ok: boolean;
    actions_completed: number;
    error: string | null;
    state: State | null;
};

export type CommandRecord = {
    command: unknown;
    reply: Reply;
    action_ms: number;
    wait_ms: number;
    observation_ms: number;
    wall_ms: number;
    observation_attempts: number;
    reply_bytes: number;
    reply_tokens: number;
};

export class Session {
    readonly records: CommandRecord[] = [];
    private busy = false;
    private terminated = false;

    constructor(
        private readonly driver: Driver,
        private readonly terminate: () => Promise<void>,
    ) {}

    async command(input: unknown): Promise<Reply> {
        if (this.busy) throw new Error("another command is still executing");
        if (this.terminated) throw new Error("browser session terminated");
        this.busy = true;
        const started = performance.now();
        const reply: Reply = {
            ok: false,
            actions_completed: 0,
            error: null,
            state: null,
        };
        const record: CommandRecord = {
            command: input,
            reply,
            action_ms: 0,
            wait_ms: 0,
            observation_ms: 0,
            wall_ms: 0,
            observation_attempts: 0,
            reply_bytes: 0,
            reply_tokens: 0,
        };
        try {
            const command = parseCommand(input);
            const deadline = performance.now() + command.timeout_ms;
            const remaining = () => Math.max(1, deadline - performance.now());
            try {
                for (const action of command.actions) {
                    if (performance.now() >= deadline) {
                        throw new Error(
                            "batch deadline reached before next action; remaining actions not executed",
                        );
                    }
                    const before = performance.now();
                    try {
                        await this.bounded(
                            this.driver.act(action, remaining()),
                            remaining() + 500,
                        );
                        reply.actions_completed++;
                    } finally {
                        record.action_ms += performance.now() - before;
                    }
                }
                if (command.wait) {
                    const before = performance.now();
                    try {
                        await this.bounded(
                            this.driver.wait(command.wait, remaining()),
                            remaining() + 500,
                        );
                    } finally {
                        record.wait_ms += performance.now() - before;
                    }
                }
            } catch (error) {
                reply.error = message(error);
            }
            if (!this.terminated) {
                const before = performance.now();
                try {
                    record.observation_attempts++;
                    reply.state = await this.bounded(
                        this.driver.observe(command.view),
                        8_000,
                    );
                } catch (error) {
                    reply.error ??= message(error);
                    if (!this.terminated && command.view.mode === "scope") {
                        record.observation_attempts++;
                        reply.state = await this.bounded(
                            this.driver.observe({ mode: "overview" }),
                            8_000,
                        );
                    }
                } finally {
                    record.observation_ms += performance.now() - before;
                }
            }
            reply.ok = reply.error === null;
        } catch (error) {
            reply.error ??= message(error);
        } finally {
            const serialized = JSON.stringify(reply) + "\n";
            record.reply_bytes = new TextEncoder().encode(serialized).length;
            record.reply_tokens = encode(serialized).length;
            record.wall_ms = performance.now() - started;
            this.records.push(record);
            this.busy = false;
        }
        return reply;
    }

    private async bounded<T>(work: Promise<T>, timeout_ms: number): Promise<T> {
        try {
            return await withDeadline("runtime supervisor", timeout_ms, work);
        } catch (error) {
            if (
                error instanceof TimeoutError &&
                error.message.startsWith("runtime supervisor")
            ) {
                this.terminated = true;
                await this.terminate();
                throw new Error(
                    "browser stopped after supervisor timeout; action outcome may be uncertain; no actions replayed",
                );
            }
            throw error;
        }
    }
}

function message(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
}
