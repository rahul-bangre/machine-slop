const BLOCK_BYTES = 512;
const MAX_USTAR_NAME_BYTES = 100;
const MAX_USTAR_SIZE = 8 ** 11 - 1;
const DEFAULT_MODE = 0o644;

export type ArchiveFile =
    | string
    | Uint8Array<ArrayBuffer>
    | { data: string | Uint8Array<ArrayBuffer>; mode: number };

export function tar(files: Record<string, ArchiveFile>): Blob {
    const encoder = new TextEncoder();
    const mtime = Math.floor(Date.now() / 1000);
    const parts: Uint8Array<ArrayBuffer>[] = [];
    for (const [path, file] of Object.entries(files)) {
        requireRelativePath(path);
        const { data, mode } =
            typeof file === "string" || file instanceof Uint8Array
                ? { data: file, mode: DEFAULT_MODE }
                : file;
        const bytes = typeof data === "string" ? encoder.encode(data) : data;
        if (bytes.byteLength > MAX_USTAR_SIZE) {
            throw new RangeError(
                `${path} is larger than an archive entry allows`,
            );
        }
        const name = encoder.encode(path);
        if (name.byteLength > MAX_USTAR_NAME_BYTES) {
            const record = paxRecord("path", path);
            parts.push(
                header(
                    encoder.encode("././@PaxHeader"),
                    record.byteLength,
                    0o644,
                    "x",
                    mtime,
                ),
            );
            parts.push(...padded(record));
        }
        parts.push(
            header(
                name.subarray(0, MAX_USTAR_NAME_BYTES),
                bytes.byteLength,
                mode,
                "0",
                mtime,
            ),
        );
        parts.push(...padded(bytes));
    }
    parts.push(new Uint8Array(BLOCK_BYTES * 2));
    return new Blob(parts, { type: "application/x-tar" });
}

function requireRelativePath(path: string): void {
    const segments = path.split("/");
    if (
        segments.some((segment) =>
            segment === "" || segment === "." || segment === ".."
        )
    ) {
        throw new TypeError(
            `archive path ${
                JSON.stringify(path)
            } must be relative, without empty, . or .. segments`,
        );
    }
}

function header(
    name: Uint8Array,
    size: number,
    mode: number,
    type: "0" | "x",
    mtime: number,
): Uint8Array<ArrayBuffer> {
    const block = new Uint8Array(BLOCK_BYTES);
    block.set(name, 0);
    octal(block, 100, 8, mode & 0o7777);
    octal(block, 108, 8, 0);
    octal(block, 116, 8, 0);
    octal(block, 124, 12, size);
    octal(block, 136, 12, mtime);
    block.fill(0x20, 148, 156);
    block[156] = type.charCodeAt(0);
    block.set(new TextEncoder().encode("ustar\u000000"), 257);
    const checksum = block.reduce((sum, byte) => sum + byte, 0);
    block.set(
        new TextEncoder().encode(
            `${checksum.toString(8).padStart(6, "0")}\u0000 `,
        ),
        148,
    );
    return block;
}

function octal(
    block: Uint8Array,
    offset: number,
    length: number,
    value: number,
): void {
    const digits = value.toString(8).padStart(length - 1, "0");
    block.set(new TextEncoder().encode(`${digits}\u0000`), offset);
}

function paxRecord(key: string, value: string): Uint8Array<ArrayBuffer> {
    const body = new TextEncoder().encode(` ${key}=${value}\n`);
    let length = body.byteLength;
    while (String(length).length + body.byteLength !== length) {
        length = String(length).length + body.byteLength;
    }
    const record = new Uint8Array(length);
    record.set(new TextEncoder().encode(String(length)));
    record.set(body, length - body.byteLength);
    return record;
}

function padded(bytes: Uint8Array<ArrayBuffer>): Uint8Array<ArrayBuffer>[] {
    const remainder = bytes.byteLength % BLOCK_BYTES;
    return remainder === 0
        ? [bytes]
        : [bytes, new Uint8Array(BLOCK_BYTES - remainder)];
}
