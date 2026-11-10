export class Machine {
    private readonly url: URL;

    constructor(url: string | URL) {
        this.url = new URL(url);
    }

    async health(signal?: AbortSignal): Promise<string> {
        const response = await fetch(new URL("/health", this.url), {
            signal: signal ?? null,
        });
        if (!response.ok) {
            throw new Error(`Health request failed: HTTP ${response.status}`);
        }
        return response.text();
    }
}
