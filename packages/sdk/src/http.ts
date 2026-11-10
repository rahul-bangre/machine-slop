export class HttpError extends Error {
    constructor(readonly status: number, message: string) {
        super(message);
        this.name = "HttpError";
    }
}

export type Endpoint = {
    readonly url: URL;
    readonly headers: Readonly<Record<string, string>>;
};

export async function request(
    endpoint: Endpoint,
    path: string,
    init: RequestInit = {},
): Promise<Response> {
    const { url } = endpoint;
    const base_path = url.pathname.replace(/\/+$/, "");
    const headers = new Headers(endpoint.headers);
    new Headers(init.headers).forEach((value, name) =>
        headers.set(name, value)
    );
    const response = await fetch(new URL(`${base_path}${path}`, url), {
        ...init,
        headers,
    });
    if (!response.ok) {
        throw new HttpError(response.status, await response.text());
    }
    return response;
}
