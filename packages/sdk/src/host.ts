import { request } from "./http.ts";
import { Machine } from "./machine.ts";
import type {
    CreateMachine,
    CreatePreview,
    CreateSnapshot,
    EgressCredential,
    MachineInfo,
    PreviewInfo,
    SnapshotInfo,
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

    setAllowedHosts(
        allowed_hosts: string[],
        signal?: AbortSignal,
    ): Promise<MachineInfo> {
        return this.update({ allowed_hosts }, signal);
    }

    setEgressCredentials(
        egress_credentials: EgressCredential[],
        signal?: AbortSignal,
    ): Promise<MachineInfo> {
        return this.update({ egress_credentials }, signal);
    }

    async preview(
        port: number,
        options: { ttl_secs?: number } = {},
        signal?: AbortSignal,
    ): Promise<PreviewInfo> {
        const preview: CreatePreview = {
            machine_id: this.id,
            port,
            ...options,
        };
        return (await request(this.host, "/previews", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(preview),
            signal: signal ?? null,
        })).json();
    }

    private async update(
        update: UpdateMachine,
        signal?: AbortSignal,
    ): Promise<MachineInfo> {
        return (await request(this.host, `/machines/${this.id}`, {
            method: "PATCH",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(update),
            signal: signal ?? null,
        })).json();
    }

    pause(signal?: AbortSignal): Promise<MachineInfo> {
        return this.transition("pause", signal);
    }

    resume(signal?: AbortSignal): Promise<MachineInfo> {
        return this.transition("resume", signal);
    }

    async fork(
        signal?: AbortSignal,
    ): Promise<{ machine: HostedMachine; info: MachineInfo }> {
        const response = await request(this.host, `/machines/${this.id}/fork`, {
            method: "POST",
            signal: signal ?? null,
        });
        const info: MachineInfo = await response.json();
        return { machine: new HostedMachine(this.host, info.id), info };
    }

    async snapshot(signal?: AbortSignal): Promise<SnapshotInfo> {
        const snapshot: CreateSnapshot = { machine_id: this.id };
        return (await request(this.host, "/snapshots", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(snapshot),
            signal: signal ?? null,
        })).json();
    }

    private async transition(
        to: "pause" | "resume",
        signal?: AbortSignal,
    ): Promise<MachineInfo> {
        return (await request(this.host, `/machines/${this.id}/${to}`, {
            method: "POST",
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

    async snapshots(signal?: AbortSignal): Promise<SnapshotInfo[]> {
        return (await request(this.url, "/snapshots", {
            signal: signal ?? null,
        })).json();
    }

    async snapshot(id: string, signal?: AbortSignal): Promise<SnapshotInfo> {
        return (await request(this.url, `/snapshots/${id}`, {
            signal: signal ?? null,
        })).json();
    }

    async deleteSnapshot(id: string, signal?: AbortSignal): Promise<void> {
        await request(this.url, `/snapshots/${id}`, {
            method: "DELETE",
            signal: signal ?? null,
        });
    }

    async previews(signal?: AbortSignal): Promise<PreviewInfo[]> {
        return (await request(this.url, "/previews", {
            signal: signal ?? null,
        })).json();
    }

    async deletePreview(id: string, signal?: AbortSignal): Promise<void> {
        await request(this.url, `/previews/${id}`, {
            method: "DELETE",
            signal: signal ?? null,
        });
    }

    machine(id: string): HostedMachine {
        return new HostedMachine(this.url, id);
    }
}
