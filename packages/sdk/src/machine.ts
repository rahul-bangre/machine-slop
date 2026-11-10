import { request } from "./http.ts";
import { Process } from "./process.ts";
import type { ProcessInfo, StartProcess } from "./process.gen.ts";

export class Machine {
    private readonly url: URL;

    constructor(url: string | URL) {
        this.url = new URL(url);
    }

    async health(signal?: AbortSignal): Promise<string> {
        return (await request(this.url, "/health", { signal: signal ?? null }))
            .text();
    }

    async startProcess(
        command: StartProcess,
        signal?: AbortSignal,
    ): Promise<Process> {
        const response = await request(this.url, "/processes", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(command),
            signal: signal ?? null,
        });
        const info: ProcessInfo = await response.json();
        return this.process(info.id);
    }

    process(id: string): Process {
        return new Process(this.url, id);
    }
}
