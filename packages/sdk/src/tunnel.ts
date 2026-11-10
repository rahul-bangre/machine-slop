import { HttpError } from "./http.ts";

const READ_CHUNK_BYTES = 64 * 1024;
const MAX_HEAD_BYTES = 16 * 1024;
const HEAD_END = new Uint8Array([13, 10, 13, 10]);

export type Tunnel = {
    readonly readable: ReadableStream<Uint8Array<ArrayBuffer>>;
    readonly writable: WritableStream<Uint8Array<ArrayBuffer>>;
    close(): void;
};

export async function openTunnel(
    base: URL,
    port: number,
    signal?: AbortSignal,
): Promise<Tunnel> {
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
        throw new RangeError(`port must be between 1 and 65535, got ${port}`);
    }
    const conn = await connect(base);
    let closed = false;
    const closeConnection = () => {
        if (closed) return;
        closed = true;
        conn.close();
    };
    signal?.addEventListener("abort", closeConnection, { once: true });
    try {
        signal?.throwIfAborted();
        const path = `${base.pathname.replace(/\/+$/, "")}/ports/${port}`;
        await writeAll(
            conn,
            new TextEncoder().encode(
                `GET ${path} HTTP/1.1\r\nHost: ${base.host}\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n`,
            ),
        );
        const { status, headers, rest } = await readHead(conn);
        if (status !== 101) {
            const length = Number(headers.get("content-length") ?? 0);
            const body = await readExactly(conn, rest, length);
            throw new HttpError(status, new TextDecoder().decode(body));
        }
        return duplex(conn, rest);
    } catch (error) {
        closeConnection();
        throw signal?.aborted ? signal.reason : error;
    } finally {
        signal?.removeEventListener("abort", closeConnection);
    }
}

export function duplex(
    conn: Deno.Conn,
    already_read = new Uint8Array(),
): Tunnel {
    let open_directions = 2;
    let closed = false;
    const close = () => {
        if (closed) return;
        closed = true;
        conn.close();
    };
    const finish = () => {
        if (--open_directions === 0) close();
    };
    const readable = new ReadableStream<Uint8Array<ArrayBuffer>>({
        start(controller) {
            if (already_read.byteLength > 0) controller.enqueue(already_read);
        },
        async pull(controller) {
            const buffer = new Uint8Array(READ_CHUNK_BYTES);
            const count = await conn.read(buffer);
            if (count === null) {
                controller.close();
                finish();
            } else {
                controller.enqueue(buffer.subarray(0, count));
            }
        },
        cancel: finish,
    });
    const writable = new WritableStream<Uint8Array<ArrayBuffer>>({
        write: (chunk) => writeAll(conn, chunk),
        async close() {
            await conn.closeWrite();
            finish();
        },
        abort: close,
    });
    return { readable, writable, close };
}

function connect(base: URL): Promise<Deno.Conn> {
    const hostname = base.hostname.replace(/^\[(.*)\]$/, "$1");
    if (base.protocol === "https:") {
        return Deno.connectTls({ hostname, port: Number(base.port || 443) });
    }
    if (base.protocol === "http:") {
        return Deno.connect({ hostname, port: Number(base.port || 80) });
    }
    throw new TypeError(`unsupported machine URL protocol ${base.protocol}`);
}

async function writeAll(
    conn: Deno.Conn,
    bytes: Uint8Array<ArrayBuffer>,
): Promise<void> {
    let written = 0;
    while (written < bytes.byteLength) {
        written += await conn.write(bytes.subarray(written));
    }
}

async function readHead(conn: Deno.Conn): Promise<{
    status: number;
    headers: Headers;
    rest: Uint8Array<ArrayBuffer>;
}> {
    let received = new Uint8Array();
    while (true) {
        const end = indexOf(received, HEAD_END);
        if (end >= 0) {
            const [status_line = "", ...lines] = new TextDecoder()
                .decode(received.subarray(0, end))
                .split("\r\n");
            const status = Number(status_line.split(" ")[1]);
            if (!status_line.startsWith("HTTP/1.1 ") || !status) {
                throw new Error(`malformed tunnel response: ${status_line}`);
            }
            const headers = new Headers();
            for (const line of lines) {
                const colon = line.indexOf(":");
                if (colon > 0) {
                    headers.append(
                        line.slice(0, colon),
                        line.slice(colon + 1).trim(),
                    );
                }
            }
            return {
                status,
                headers,
                rest: received.slice(end + HEAD_END.length),
            };
        }
        if (received.byteLength > MAX_HEAD_BYTES) {
            throw new Error("tunnel response head is too large");
        }
        received = concat(received, await readSome(conn));
    }
}

async function readExactly(
    conn: Deno.Conn,
    already_read: Uint8Array<ArrayBuffer>,
    length: number,
): Promise<Uint8Array<ArrayBuffer>> {
    let received = already_read;
    while (received.byteLength < Math.min(length, MAX_HEAD_BYTES)) {
        received = concat(received, await readSome(conn));
    }
    return received.subarray(0, length);
}

async function readSome(conn: Deno.Conn): Promise<Uint8Array<ArrayBuffer>> {
    const buffer = new Uint8Array(READ_CHUNK_BYTES);
    const count = await conn.read(buffer);
    if (count === null) {
        throw new Error("connection closed during tunnel setup");
    }
    return buffer.subarray(0, count);
}

function concat(
    a: Uint8Array<ArrayBuffer>,
    b: Uint8Array<ArrayBuffer>,
): Uint8Array<ArrayBuffer> {
    const joined = new Uint8Array(a.byteLength + b.byteLength);
    joined.set(a);
    joined.set(b, a.byteLength);
    return joined;
}

function indexOf(
    haystack: Uint8Array<ArrayBuffer>,
    needle: Uint8Array<ArrayBuffer>,
): number {
    outer: for (let i = 0; i + needle.byteLength <= haystack.byteLength; i++) {
        for (let j = 0; j < needle.byteLength; j++) {
            if (haystack[i + j] !== needle[j]) continue outer;
        }
        return i;
    }
    return -1;
}
