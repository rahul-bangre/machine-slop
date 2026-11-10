import { Forward } from "./forward.ts";
import { request } from "./http.ts";
import { Process } from "./process.ts";
import type { ProcessInfo, StartProcess } from "./process.gen.ts";
import { openTunnel, type Tunnel } from "./tunnel.ts";

export type FileData =
    | string
    | Uint8Array<ArrayBuffer>
    | Blob
    | ReadableStream<Uint8Array<ArrayBuffer>>;

export type WriteFileOptions = { mode?: number; signal?: AbortSignal };

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

    async readFile(
        path: string,
        signal?: AbortSignal,
    ): Promise<Uint8Array<ArrayBuffer>> {
        const response = await request(this.url, machinePath("files", path), {
            signal: signal ?? null,
        });
        return new Uint8Array(await response.arrayBuffer());
    }

    async writeFile(
        path: string,
        data: FileData,
        options: WriteFileOptions = {},
    ): Promise<void> {
        const mode = options.mode === undefined
            ? ""
            : `?mode=${options.mode.toString(8)}`;
        await request(this.url, `${machinePath("files", path)}${mode}`, {
            method: "PUT",
            headers: { "content-type": "application/octet-stream" },
            body: data,
            signal: options.signal ?? null,
        });
    }

    async remove(path: string, signal?: AbortSignal): Promise<void> {
        await request(this.url, machinePath("files", path), {
            method: "DELETE",
            signal: signal ?? null,
        });
    }

    async readArchive(
        directory: string,
        signal?: AbortSignal,
    ): Promise<ReadableStream<Uint8Array<ArrayBuffer>>> {
        const response = await request(
            this.url,
            machinePath("archives", directory),
            { signal: signal ?? null },
        );
        if (!response.body) throw new Error("Missing archive stream");
        return response.body;
    }

    async writeArchive(
        directory: string,
        archive: Exclude<FileData, string>,
        signal?: AbortSignal,
    ): Promise<void> {
        await request(this.url, machinePath("archives", directory), {
            method: "PUT",
            headers: { "content-type": "application/x-tar" },
            body: archive,
            signal: signal ?? null,
        });
    }

    connect(port: number, signal?: AbortSignal): Promise<Tunnel> {
        return openTunnel(this.url, port, signal);
    }

    forward(port: number, local_port = 0): Forward {
        const listener = Deno.listen({
            hostname: "127.0.0.1",
            port: local_port,
        });
        return new Forward(listener, this.url, port);
    }
}

function machinePath(resource: "files" | "archives", path: string): string {
    const [root, ...segments] = path.split("/");
    if (
        root !== "" || segments.length === 0 ||
        segments.some((segment) => ["", ".", ".."].includes(segment))
    ) {
        throw new TypeError(
            `machine path ${
                JSON.stringify(path)
            } must be absolute, without empty, . or .. segments`,
        );
    }
    return `/${resource}/${segments.map(encodeURIComponent).join("/")}`;
}
