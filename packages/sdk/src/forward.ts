import { duplex, openTunnel, type Tunnel } from "./tunnel.ts";

export class Forward {
    private readonly open = new Set<Tunnel>();
    private readonly serving: Promise<void>;

    constructor(
        private readonly listener: Deno.Listener<Deno.TcpConn, Deno.NetAddr>,
        machine: URL,
        port: number,
    ) {
        this.serving = this.serve(machine, port);
    }

    get hostname(): string {
        return this.listener.addr.hostname;
    }

    get port(): number {
        return this.listener.addr.port;
    }

    get url(): string {
        return `http://${this.hostname}:${this.port}`;
    }

    async close(): Promise<void> {
        this.listener.close();
        for (const tunnel of this.open) tunnel.close();
        await this.serving;
    }

    private async serve(machine: URL, port: number): Promise<void> {
        const pending = new Set<Promise<void>>();
        try {
            for await (const conn of this.listener) {
                const piped = this.pipe(duplex(conn), machine, port).catch(
                    () => {},
                );
                pending.add(piped);
                piped.finally(() => pending.delete(piped));
            }
        } catch (error) {
            if (!(error instanceof Deno.errors.BadResource)) throw error;
        }
        await Promise.allSettled(pending);
    }

    private async pipe(
        local: Tunnel,
        machine: URL,
        port: number,
    ): Promise<void> {
        const tunnels = [local];
        this.open.add(local);
        try {
            const remote = await openTunnel(machine, port);
            tunnels.push(remote);
            this.open.add(remote);
            await Promise.all([
                local.readable.pipeTo(remote.writable),
                remote.readable.pipeTo(local.writable),
            ]);
        } finally {
            for (const tunnel of tunnels) {
                this.open.delete(tunnel);
                tunnel.close();
            }
        }
    }
}
