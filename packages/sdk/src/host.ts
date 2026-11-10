import { request } from "./http.ts";
import { Machine } from "./machine.ts";
import type {
    CreateMachine,
    MachineInfo,
    UpdateMachine,
} from "./machines.gen.ts";

export class HostedMachine extends Machine {
    constructor(private readonly host: URL, readonly id: string) {
        super(new URL(`machines/${id}`, host));
    }

    async info(signal?: AbortSignal): Promise<MachineInfo> {
        return (await request(this.host, `/machines/${this.id}`, {
            signal: signal ?? null,
        })).json();
    }

    async setAllowedHosts(
        allowed_hosts: string[],
        signal?: AbortSignal,
    ): Promise<MachineInfo> {
        const update: UpdateMachine = { allowed_hosts };
        return (await request(this.host, `/machines/${this.id}`, {
            method: "PATCH",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(update),
            signal: signal ?? null,
        })).json();
    }

    async destroy(signal?: AbortSignal): Promise<void> {
        await request(this.host, `/machines/${this.id}`, {
            method: "DELETE",
            signal: signal ?? null,
        });
    }
}

export class Host {
    private readonly url: URL;

    constructor(url: string | URL) {
        this.url = new URL(url);
        if (!this.url.pathname.endsWith("/")) this.url.pathname += "/";
    }

    async createMachine(
        options: CreateMachine = {},
        signal?: AbortSignal,
    ): Promise<{ machine: HostedMachine; info: MachineInfo }> {
        const response = await request(this.url, "/machines", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(options),
            signal: signal ?? null,
        });
        const info: MachineInfo = await response.json();
        return { machine: this.machine(info.id), info };
    }

    async machines(signal?: AbortSignal): Promise<MachineInfo[]> {
        return (await request(this.url, "/machines", {
            signal: signal ?? null,
        })).json();
    }

    machine(id: string): HostedMachine {
        return new HostedMachine(this.url, id);
    }
}
