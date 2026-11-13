import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { watch } from "node:fs";
import { extname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { cart, shipping_cents, subtotalCents } from "./lib/cart.js";

const public_dir = fileURLToPath(new URL("./public/", import.meta.url));
const content_types = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
};
const reload_clients = new Set();
const started_at = Date.now();

function sendJson(response, status, body) {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify(body));
}

async function readJson(request) {
  let body = "";
  for await (const chunk of request) body += chunk;
  return body ? JSON.parse(body) : {};
}

export function quote() {
  const subtotal_cents = subtotalCents();
  return {
    items: cart,
    subtotal_cents,
    discount_cents: 0,
    shipping_cents,
    total_cents: subtotal_cents + shipping_cents,
  };
}

export function createApp() {
  return createServer(async (request, response) => {
    const url = new URL(request.url, "http://localhost");
    if (url.pathname === "/__reload") {
      response.writeHead(200, {
        "content-type": "text/event-stream",
        "cache-control": "no-store",
      });
      response.write("data: hello\n\n");
      reload_clients.add(response);
      request.on("close", () => reload_clients.delete(response));
      return;
    }
    if (url.pathname === "/__dev") {
      return sendJson(response, 200, {
        pid: process.pid,
        uptime_ms: Date.now() - started_at,
      });
    }
    if (url.pathname === "/api/cart" && request.method === "GET") {
      return sendJson(response, 200, quote());
    }
    if (url.pathname === "/api/checkout" && request.method === "POST") {
      await readJson(request);
      return sendJson(response, 200, quote());
    }
    const path = url.pathname === "/" ? "index.html" : url.pathname.slice(1);
    try {
      const body = await readFile(join(public_dir, path));
      response.writeHead(200, {
        "content-type": content_types[extname(path)] ??
          "application/octet-stream",
        "cache-control": "no-store",
      });
      response.end(body);
    } catch {
      response.writeHead(404).end("not found");
    }
  });
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  watch(public_dir, { recursive: true }, () => {
    for (const client of reload_clients) client.write("data: reload\n\n");
  });
  const port = Number(process.env.PORT ?? 3000);
  createApp().listen(port, () => console.log(`slop supply on :${port}`));
}
