import { type Endpoint, request } from "./http.ts";
import { Machine } from "./machine.ts";
import type {
    CreateMachine,
    CreatePreview,
    CreateSnapshot,
    EgressCredential,
    MachineInfo,
    PreviewInfo,
    SnapshotInfo,
    TenantUsage,
    UpdateMachine,
} from "./machines.gen.ts";

export type HostOptions = { apiKey: string };

function retrySafe(idempotencyKey: string | undefined): Record<string, string> {
    return idempotencyKey === undefined
        ? {}
        : { "idempotency-key": idempotencyKey };
}

export class HostedMachine extends Machine {
    constructor(private readonly host: Endpoint, readonly id: string) {
        super(new URL(`machines/${id}`, host.url), host.headers);
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
        idempotencyKey?: string,
    ): Promise<{ machine: HostedMachine; info: MachineInfo }> {
        const response = await request(this.host, `/machines/${this.id}/fork`, {
            method: "POST",
            headers: retrySafe(idempotencyKey),
            signal: signal ?? null,
        });
        const info: MachineInfo = await response.json();
        return { machine: new HostedMachine(this.host, info.id), info };
    }

    async snapshot(
        signal?: AbortSignal,
        idempotencyKey?: string,
    ): Promise<SnapshotInfo> {
        const snapshot: CreateSnapshot = { machine_id: this.id };
        return (await request(this.host, "/snapshots", {
            method: "POST",
            headers: {
                "content-type": "application/json",
                ...retrySafe(idempotencyKey),
            },
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
    private readonly endpoint: Endpoint;

    constructor(url: string | URL, options: HostOptions) {
        const base = new URL(url);
        if (!base.pathname.endsWith("/")) base.pathname += "/";
        this.endpoint = {
            url: base,
            headers: { authorization: `Bearer ${options.apiKey}` },
        };
    }

    async createMachine(
        options: CreateMachine = {},
        signal?: AbortSignal,
        idempotencyKey?: string,
    ): Promise<{ machine: HostedMachine; info: MachineInfo }> {
        const response = await request(this.endpoint, "/machines", {
            method: "POST",
            headers: {
                "content-type": "application/json",
                ...retrySafe(idempotencyKey),
            },
            body: JSON.stringify(options),
            signal: signal ?? null,
        });
        const info: MachineInfo = await response.json();
        return { machine: this.machine(info.id), info };
    }

    async machines(signal?: AbortSignal): Promise<MachineInfo[]> {
        return (await request(this.endpoint, "/machines", {
            signal: signal ?? null,
        })).json();
    }

    async snapshots(signal?: AbortSignal): Promise<SnapshotInfo[]> {
        return (await request(this.endpoint, "/snapshots", {
            signal: signal ?? null,
        })).json();
    }

    async snapshot(id: string, signal?: AbortSignal): Promise<SnapshotInfo> {
        return (await request(this.endpoint, `/snapshots/${id}`, {
            signal: signal ?? null,
        })).json();
    }

    async deleteSnapshot(id: string, signal?: AbortSignal): Promise<void> {
        await request(this.endpoint, `/snapshots/${id}`, {
            method: "DELETE",
            signal: signal ?? null,
        });
    }

    async previews(signal?: AbortSignal): Promise<PreviewInfo[]> {
        return (await request(this.endpoint, "/previews", {
            signal: signal ?? null,
        })).json();
    }

    async deletePreview(id: string, signal?: AbortSignal): Promise<void> {
        await request(this.endpoint, `/previews/${id}`, {
            method: "DELETE",
            signal: signal ?? null,
        });
    }

    machine(id: string): HostedMachine {
        return new HostedMachine(this.endpoint, id);
    }

    async usage(signal?: AbortSignal): Promise<TenantUsage> {
        return (await request(this.endpoint, "/usage", {
            signal: signal ?? null,
        })).json();
    }
}
