export class CdpError extends Error {
    constructor(readonly method: string, message: string) {
        super(`${method}: ${message}`);
        this.name = "CdpError";
    }
}

export class TimeoutError extends Error {
    constructor(label: string, ms: number) {
        super(`${label} exceeded ${ms}ms`);
        this.name = "TimeoutError";
    }
}

export async function withDeadline<T>(
    label: string,
    ms: number,
    work: Promise<T>,
): Promise<T> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const expired = new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new TimeoutError(label, ms)), ms);
    });
    try {
        return await Promise.race([work, expired]);
    } finally {
        clearTimeout(timer);
    }
}

export type CdpEvent = {
    method: string;
    params: Record<string, unknown>;
    sessionId?: string;
};

type Pending = {
    method: string;
    resolve: (result: unknown) => void;
    reject: (error: Error) => void;
};

export class Cdp {
    private next_id = 1;
    private readonly pending = new Map<number, Pending>();
    private readonly listeners = new Set<(event: CdpEvent) => void>();

    private constructor(private readonly socket: WebSocket) {
        socket.addEventListener("message", (message) => {
            this.dispatch(JSON.parse(String(message.data)));
        });
        socket.addEventListener("close", () => {
            for (const { method, reject } of this.pending.values()) {
                reject(new CdpError(method, "connection closed"));
            }
            this.pending.clear();
        });
    }

    static connect(url: string): Promise<Cdp> {
        const socket = new WebSocket(url);
        return new Promise((resolve, reject) => {
            socket.addEventListener("open", () => resolve(new Cdp(socket)));
            socket.addEventListener(
                "error",
                () => reject(new Error(`cannot connect to ${url}`)),
            );
        });
    }

    send<T>(
        method: string,
        params: Record<string, unknown> = {},
        sessionId?: string,
    ): Promise<T> {
        const id = this.next_id++;
        return new Promise<T>((resolve, reject) => {
            if (this.socket.readyState !== WebSocket.OPEN) {
                reject(new CdpError(method, "connection closed"));
                return;
            }
            this.pending.set(id, {
                method,
                resolve: resolve as (result: unknown) => void,
                reject,
            });
            this.socket.send(JSON.stringify({ id, method, params, sessionId }));
        });
    }

    listen(listener: (event: CdpEvent) => void): () => void {
        this.listeners.add(listener);
        return () => this.listeners.delete(listener);
    }

    session(sessionId: string): CdpSession {
        return new CdpSession(this, sessionId);
    }

    close(): void {
        this.socket.close();
    }

    private dispatch(message: {
        id?: number;
        result?: unknown;
        error?: { message: string };
        method?: string;
        params?: Record<string, unknown>;
        sessionId?: string;
    }): void {
        if (message.id === undefined) {
            if (!message.method) return;
            const event: CdpEvent = {
                method: message.method,
                params: message.params ?? {},
                ...(message.sessionId ? { sessionId: message.sessionId } : {}),
            };
            for (const listener of this.listeners) listener(event);
            return;
        }
        const pending = this.pending.get(message.id);
        if (!pending) return;
        this.pending.delete(message.id);
        if (message.error) {
            pending.reject(new CdpError(pending.method, message.error.message));
        } else {
            pending.resolve(message.result);
        }
    }
}

export class CdpSession {
    constructor(private readonly cdp: Cdp, readonly id: string) {}

    send<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
        return this.cdp.send<T>(method, params, this.id);
    }

    on(
        method: string,
        handler: (params: Record<string, unknown>) => void,
    ): () => void {
        return this.cdp.listen((event) => {
            if (event.sessionId === this.id && event.method === method) {
                handler(event.params);
            }
        });
    }
}
