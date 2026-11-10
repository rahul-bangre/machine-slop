import { type Endpoint, request } from "./http.ts";
import type {
    ProcessEvent,
    ProcessInfo,
    ProcessResult,
    TerminalSize,
} from "./process.gen.ts";

export type OutputEvent = Exclude<ProcessEvent, { type: "gap" }>;

export class OutputGap extends Error {
    constructor(readonly first_seq: number) {
        super(`Process output expired; first available cursor is ${first_seq}`);
        this.name = "OutputGap";
    }
}

export class Process {
    constructor(private readonly endpoint: Endpoint, readonly id: string) {}

    async info(signal?: AbortSignal): Promise<ProcessInfo> {
        return (await request(this.endpoint, this.path(), {
            signal: signal ?? null,
        }))
            .json();
    }

    async wait(signal?: AbortSignal): Promise<ProcessResult> {
        return (await request(this.endpoint, `${this.path()}/wait`, {
            signal: signal ?? null,
        })).json();
    }

    async kill(signal?: AbortSignal): Promise<void> {
        await request(this.endpoint, `${this.path()}/kill`, {
            method: "POST",
            signal: signal ?? null,
        });
    }

    async writeStdin(
        offset: number,
        data: Uint8Array<ArrayBuffer> | string,
        signal?: AbortSignal,
    ): Promise<number> {
        const bytes = typeof data === "string"
            ? new TextEncoder().encode(data)
            : data;
        await this.postStdin(`offset=${offset}`, bytes, signal);
        return offset + bytes.byteLength;
    }

    async closeStdin(offset: number, signal?: AbortSignal): Promise<void> {
        await this.postStdin(
            `offset=${offset}&close=true`,
            new Uint8Array(),
            signal,
        );
    }

    async resize(size: TerminalSize, signal?: AbortSignal): Promise<void> {
        await request(this.endpoint, `${this.path()}/terminal`, {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(size),
            signal: signal ?? null,
        });
    }

    async *events(
        cursor = 0,
        signal?: AbortSignal,
    ): AsyncGenerator<OutputEvent> {
        const response = await request(
            this.endpoint,
            `${this.path()}/events?cursor=${cursor}`,
            { signal: signal ?? null },
        );
        if (response.status === 204) return;
        if (!response.body) throw new Error("Missing process event stream");
        const parser = new EventParser(cursor);
        for await (
            const chunk of response.body.pipeThrough(new TextDecoderStream())
        ) {
            for (const event of parser.push(chunk)) yield event;
        }
        parser.finish();
    }

    private async postStdin(
        query: string,
        body: Uint8Array<ArrayBuffer>,
        signal?: AbortSignal,
    ): Promise<void> {
        await request(this.endpoint, `${this.path()}/stdin?${query}`, {
            method: "POST",
            headers: { "content-type": "application/octet-stream" },
            body,
            signal: signal ?? null,
        });
    }

    private path(): string {
        return `/processes/${encodeURIComponent(this.id)}`;
    }
}

class EventParser {
    private buffer = "";
    private finished = false;

    constructor(private cursor: number) {}

    push(chunk: string): OutputEvent[] {
        const lines = (this.buffer + chunk).split("\n");
        this.buffer = lines.pop() ?? "";
        return lines.map((line) => {
            const event: ProcessEvent = JSON.parse(line);
            if (event.type === "gap") throw new OutputGap(event.first_seq);
            if (
                this.finished || event.seq !== this.cursor ||
                !["stdout", "stderr", "finished"].includes(event.type)
            ) {
                throw new Error("Invalid process event sequence");
            }
            this.cursor++;
            this.finished = event.type === "finished";
            return event;
        });
    }

    finish(): void {
        if (this.buffer || !this.finished) {
            throw new Error("Process event stream ended before completion");
        }
    }
}
