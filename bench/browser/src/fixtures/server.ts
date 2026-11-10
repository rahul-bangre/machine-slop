import {
    accounts,
    changelog,
    fixture_origin,
    products,
    type RequestRecord,
} from "./data.ts";
import { renderDocs } from "./docs.ts";

const pages: Record<string, string> = {
    "/shop/": await Deno.readTextFile(new URL("shop.html", import.meta.url)),
    "/dashboard/": await Deno.readTextFile(
        new URL("dashboard.html", import.meta.url),
    ),
};

const api_latency_ms: Record<string, number> = {
    "/api/products": 150,
    "/api/accounts": 200,
    "/api/changelog": 100,
    "/api/orders": 120,
    "/api/tickets": 120,
};

function html(body: string): Response {
    return new Response(body, {
        headers: { "content-type": "text/html; charset=utf-8" },
    });
}

export class FixtureServer {
    private requests: RequestRecord[] = [];
    private readonly server: Deno.HttpServer<Deno.NetAddr>;

    constructor(origin = fixture_origin) {
        const { port, hostname } = new URL(origin);
        this.server = Deno.serve(
            { port: Number(port), hostname, onListen: () => {} },
            (request) => this.handle(request),
        );
    }

    get origin(): string {
        return `http://127.0.0.1:${this.server.addr.port}`;
    }

    startRecording(): void {
        this.requests = [];
    }

    recorded(): RequestRecord[] {
        return this.requests.filter((request) =>
            request.path !== "/favicon.ico"
        );
    }

    async stop(): Promise<void> {
        await this.server.shutdown();
    }

    private async handle(request: Request): Promise<Response> {
        const path = new URL(request.url).pathname;
        const body = request.method === "POST" ? await request.json() : null;
        const record = { method: request.method, path, body };
        this.requests.push(record);

        const latency = api_latency_ms[path];
        if (latency !== undefined) {
            await new Promise((resolve) => setTimeout(resolve, latency));
        }

        const page = pages[path] ?? renderDocs(path);
        if (request.method === "GET" && page) return html(page);

        const route = `${request.method} ${path}`;
        if (route === "GET /api/products") return Response.json(products);
        if (route === "GET /api/accounts") return Response.json(accounts);
        if (route === "GET /api/changelog") return Response.json(changelog);
        if (route === "POST /api/orders") {
            return Response.json({
                order_id: `ORD-${1000 + this.count(route)}`,
            });
        }
        if (route === "POST /api/tickets") {
            return Response.json({ ticket_id: `TCK-${this.count(route)}` });
        }
        return new Response(null, { status: 404 });
    }

    private count(route: string): number {
        return this.requests.filter((request) =>
            `${request.method} ${request.path}` === route
        ).length;
    }
}
