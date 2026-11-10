export class HttpError extends Error {
    constructor(readonly status: number, message: string) {
        super(message);
        this.name = "HttpError";
    }
}

export async function request(
    url: URL,
    path: string,
    init?: RequestInit,
): Promise<Response> {
    const response = await fetch(new URL(path, url), init);
    if (!response.ok) {
        throw new HttpError(response.status, await response.text());
    }
    return response;
}
