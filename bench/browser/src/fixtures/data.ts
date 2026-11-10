export const fixture_origin = "http://127.0.0.1:41730";

export type RequestRecord = { method: string; path: string; body: unknown };

export type Product = { id: string; name: string; price_cents: number };
export type Account = {
    id: string;
    name: string;
    region: string;
    seats: number;
    revenue: number;
};

const adjectives = [
    "Copper",
    "Linen",
    "Walnut",
    "Slate",
    "Amber",
    "Cobalt",
    "Ivory",
    "Moss",
];
const nouns = ["Kettle", "Lamp", "Notebook", "Backpack", "Mug", "Blanket"];
const regions = ["AMER", "EMEA", "APAC", "LATAM"];

export const products: Product[] = nouns.flatMap((noun, row) =>
    adjectives.map((adjective, column) => {
        const index = row * adjectives.length + column;
        return {
            id: `p${index}`,
            name: `${adjective} ${noun}`,
            price_cents: 1200 + index * 135,
        };
    })
);

export const accounts: Account[] = Array.from({ length: 1000 }, (_, index) => ({
    id: `a${index + 1}`,
    name: `Account ${String(index + 1).padStart(4, "0")}`,
    region: regions[index % regions.length]!,
    seats: 5 + (index * 31) % 240,
    revenue: (index + 1) * 7919 % 100_003,
}));

export const highest_revenue_account = accounts.reduce((top, account) =>
    account.revenue > top.revenue ? account : top
);

export const changelog = [
    "Raised sustained limit to 600 requests per minute",
    "Added burst allowance headers to every response",
    "Deprecated per-key hourly quotas",
    "Introduced regional rate limit pools",
    "Published retry guidance for 429 responses",
];

export const docs_pages = [
    { slug: "getting-started", title: "Getting started" },
    { slug: "authentication", title: "Authentication" },
    { slug: "rate-limits", title: "Rate limits" },
    { slug: "webhooks", title: "Webhooks" },
    { slug: "pagination", title: "Pagination" },
    { slug: "errors", title: "Errors" },
];

export function formatPrice(price_cents: number): string {
    return `$${(price_cents / 100).toFixed(2)}`;
}
