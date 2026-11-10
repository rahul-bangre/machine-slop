export function encodeHex(buffer: ArrayBuffer): string {
    return [...new Uint8Array(buffer)]
        .map((byte) => byte.toString(16).padStart(2, "0"))
        .join("");
}

export async function sha256Hex(
    bytes: Uint8Array<ArrayBuffer>,
): Promise<string> {
    return encodeHex(await crypto.subtle.digest("SHA-256", bytes));
}
