export type Capability =
    | "async-loading"
    | "dialogs"
    | "multi-step-form"
    | "recovery";

export type WorkflowSite = {
    handle(request: Request): Promise<Response>;
    outcome(): Promise<string | null>;
};

type SiteState = {
    handle(request: Request): Promise<Response>;
    outcome(): string | null;
};

export function settledSite(state: SiteState): WorkflowSite {
    const in_flight = new Set<Promise<Response>>();
    return {
        handle(request) {
            const handled = state.handle(request);
            const forget = () => in_flight.delete(handled);
            in_flight.add(handled);
            handled.then(forget, forget);
            return handled;
        },
        async outcome() {
            while (in_flight.size > 0) await Promise.allSettled(in_flight);
            return state.outcome();
        },
    };
}

export type Workflow = {
    id: string;
    covers: readonly Capability[];
    seeds: readonly number[];
    open(seed: number): { prompt: string; site: WorkflowSite };
};

export type Random = {
    next(): number;
    int(min: number, max: number): number;
    pick<T>(items: readonly T[]): T;
    shuffle<T>(items: readonly T[]): T[];
    sample<T>(items: readonly T[], count: number): T[];
};

export function seededRandom(seed: number): Random {
    let state = seed >>> 0;
    const next = () => {
        state = (state + 0x6d2b79f5) >>> 0;
        let value = state;
        value = Math.imul(value ^ (value >>> 15), value | 1);
        value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
        return ((value ^ (value >>> 14)) >>> 0) / 4294967296;
    };
    const int = (min: number, max: number) =>
        min + Math.floor(next() * (max - min + 1));
    const shuffle = <T>(items: readonly T[]): T[] => {
        const copy = [...items];
        for (let index = copy.length - 1; index > 0; index--) {
            const other = int(0, index);
            [copy[index], copy[other]] = [copy[other]!, copy[index]!];
        }
        return copy;
    };
    return {
        next,
        int,
        pick: <T>(items: readonly T[]): T => items[int(0, items.length - 1)]!,
        shuffle,
        sample: <T>(items: readonly T[], count: number): T[] =>
            shuffle(items).slice(0, count),
    };
}

export function pause(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
}

export function routeOf(
    request: Request,
): { method: string; path: string; query: URLSearchParams } {
    const url = new URL(request.url);
    return {
        method: request.method,
        path: url.pathname,
        query: url.searchParams,
    };
}

export async function readObject(
    request: Request,
): Promise<Record<string, unknown> | null> {
    try {
        const value: unknown = await request.json();
        return typeof value === "object" && value !== null &&
                !Array.isArray(value)
            ? value as Record<string, unknown>
            : null;
    } catch {
        return null;
    }
}

export function htmlResponse(body: string): Response {
    return new Response(body, {
        headers: { "content-type": "text/html; charset=utf-8" },
    });
}

export function errorResponse(
    status: number,
    error: string,
    fields: Record<string, string> = {},
): Response {
    return Response.json({ error, fields }, { status });
}

export function notFound(): Response {
    return errorResponse(404, "Not found");
}

export function firstDifference(
    label: string,
    expected: ReadonlyMap<string, unknown>,
    actual: ReadonlyMap<string, unknown>,
): string | null {
    for (const key of new Set([...expected.keys(), ...actual.keys()])) {
        const wanted = JSON.stringify(expected.get(key) ?? null);
        const found = JSON.stringify(actual.get(key) ?? null);
        if (wanted !== found) {
            return `${label} ${key}: expected ${wanted}, found ${found}`;
        }
    }
    return null;
}

const month_names = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const weekday_names = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

export function monthName(month: number): string {
    return month_names[month - 1]!;
}

export function twoDigits(value: number): string {
    return String(value).padStart(2, "0");
}

export function isoDate(year: number, month: number, day: number): string {
    return `${year}-${twoDigits(month)}-${twoDigits(day)}`;
}

export function shiftDate(iso: string, days: number): string {
    const date = new Date(`${iso}T00:00:00Z`);
    date.setUTCDate(date.getUTCDate() + days);
    return date.toISOString().slice(0, 10);
}

export function isIsoDate(value: unknown): value is string {
    if (typeof value !== "string" || !/^\d{4}-\d{2}-\d{2}$/.test(value)) {
        return false;
    }
    const date = new Date(`${value}T00:00:00Z`);
    return !Number.isNaN(date.getTime()) &&
        date.toISOString().slice(0, 10) === value;
}

export function longDate(iso: string): string {
    const date = new Date(`${iso}T00:00:00Z`);
    return `${date.getUTCDate()} ${
        monthName(date.getUTCMonth() + 1)
    } ${date.getUTCFullYear()}`;
}

export function shortDate(iso: string): string {
    const date = new Date(`${iso}T00:00:00Z`);
    return `${date.getUTCDate()} ${
        monthName(date.getUTCMonth() + 1).slice(0, 3)
    } ${date.getUTCFullYear()}`;
}

export function weekdayDate(iso: string): string {
    const date = new Date(`${iso}T00:00:00Z`);
    return `${weekday_names[date.getUTCDay()]} ${longDate(iso)}`;
}

const currency_prefixes: Record<string, string> = {
    EUR: "€",
    GBP: "£",
    USD: "$",
    SEK: "SEK ",
};

export function formatMoney(cents: number, currency: string): string {
    const whole = Math.floor(cents / 100).toString().replace(
        /\B(?=(\d{3})+(?!\d))/g,
        ",",
    );
    const fraction = twoDigits(cents % 100);
    return `${
        currency_prefixes[currency] ?? `${currency} `
    }${whole}.${fraction}`;
}

const base_style = `
  :root { font-family: system-ui, -apple-system, "Segoe UI", sans-serif; color: #1d232a; background: #f6f7f9; line-height: 1.45; }
  body { margin: 0; }
  header { background: #1f3a5f; color: #fff; padding: 12px 24px; }
  main { max-width: 980px; margin: 0 auto; padding: 24px; }
  table { border-collapse: collapse; width: 100%; background: #fff; }
  th, td { border-bottom: 1px solid #dde1e6; padding: 8px 10px; text-align: left; }
  button { font: inherit; padding: 6px 12px; border: 1px solid #8a94a3; border-radius: 4px; background: #fff; color: #1d232a; cursor: pointer; }
  button.primary { background: #1f5fbf; border-color: #1f5fbf; color: #fff; }
  button.danger { background: #b42318; border-color: #b42318; color: #fff; }
  button:disabled { opacity: 0.5; cursor: not-allowed; }
  dialog { border: none; border-radius: 8px; padding: 24px; width: min(520px, 90vw); box-shadow: 0 10px 40px rgba(0, 0, 0, 0.3); }
  dialog::backdrop { background: rgba(15, 23, 42, 0.55); }
  label { display: block; margin: 12px 0 4px; font-weight: 600; }
  input[type=text], input[type=email], input[type=password] { font: inherit; padding: 6px 8px; width: 100%; box-sizing: border-box; border: 1px solid #8a94a3; border-radius: 4px; }
  fieldset { border: 1px solid #dde1e6; border-radius: 6px; margin: 12px 0; padding: 8px 12px; }
  fieldset label { display: flex; gap: 8px; align-items: center; font-weight: 400; margin: 6px 0; }
  .error { color: #b42318; }
  .notice { color: #146c43; }
  .hint { color: #5b6470; font-size: 0.9em; }
  .actions { display: flex; gap: 8px; justify-content: flex-end; margin-top: 16px; }
  .visually-hidden { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0); white-space: nowrap; }
  [hidden] { display: none !important; }
`;

const client_helpers = `
async function api(method, path, body) {
    const init = { method, headers: {} };
    if (body !== undefined) {
        init.headers["content-type"] = "application/json";
        init.body = JSON.stringify(body);
    }
    const response = await fetch(path, init);
    const data = await response.json().catch(() => null);
    return { status: response.status, ok: response.ok, data };
}
function h(tag, attributes, ...children) {
    const node = document.createElement(tag);
    for (const [name, value] of Object.entries(attributes || {})) {
        if (value === false || value === null || value === undefined) continue;
        if (name.startsWith("on")) node.addEventListener(name.slice(2), value);
        else if (value === true) node.setAttribute(name, "");
        else node.setAttribute(name, String(value));
    }
    for (const child of children.flat()) {
        if (child === null || child === undefined || child === false) continue;
        node.append(child instanceof Node ? child : String(child));
    }
    return node;
}
function byId(id) {
    return document.getElementById(id);
}
`;

export function renderPage(page: {
    title: string;
    brand: string;
    style?: string;
    body: string;
    script: string;
}): string {
    return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <title>${page.title}</title>
    <style>${base_style}${page.style ?? ""}</style>
  </head>
  <body>
    <header><strong>${page.brand}</strong></header>
    ${page.body}
    <script>${client_helpers}${page.script}</script>
  </body>
</html>`;
}
