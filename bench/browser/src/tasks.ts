import {
    fixture_origin,
    formatPrice,
    highest_revenue_account,
    products,
    type RequestRecord,
} from "./fixtures/data.ts";
import type { Action } from "./page.ts";

export type Step = {
    name: string;
    act: Action;
    ready: string;
    observed?: string[];
};

export type Outcome =
    | {
        source: "server-request-log";
        failure(requests: RequestRecord[]): string | null;
    }
    | { source: "page-state"; holds: string };

export type Task = {
    id: string;
    content: "pinned-fixture" | "live-unpinned";
    steps: Step[];
    outcome: Outcome;
};

const sameJson = (left: unknown, right: unknown) =>
    JSON.stringify(left) === JSON.stringify(right);

function soleRequest(
    requests: RequestRecord[],
    method: string,
    path: string,
): RequestRecord | string {
    const matches = requests.filter((request) =>
        request.method === method && request.path === path
    );
    return matches.length === 1
        ? matches[0]!
        : `expected one ${method} ${path}, server saw ${matches.length}`;
}

const cobalt_notebook = products.find((product) =>
    product.name === "Cobalt Notebook"
)!;
const customer = { name: "Ada Lovelace", email: "ada@example.com" };
const ticket_note = "Renewal at risk";

const shop_checkout: Task = {
    id: "shop-checkout",
    content: "pinned-fixture",
    steps: [
        {
            name: "open shop",
            act: { goto: `${fixture_origin}/shop/` },
            ready:
                `document.querySelectorAll(".product").length === ${products.length}`,
            observed: [cobalt_notebook.name, "Search products"],
        },
        {
            name: "search",
            act: { fill: "#search", text: "cobalt note" },
            ready:
                `document.querySelectorAll(".product:not([hidden])").length === 1`,
            observed: [
                cobalt_notebook.name,
                formatPrice(cobalt_notebook.price_cents),
            ],
        },
        {
            name: "add to cart",
            act: { click: ".product:not([hidden]) .add" },
            ready: `document.querySelector("#cart-count").textContent === "1"`,
            observed: [`Added ${cobalt_notebook.name} to cart`],
        },
        {
            name: "open cart",
            act: { click: "#cart-button" },
            ready: `document.querySelector("#cart").open`,
            observed: ["Your cart", "Checkout"],
        },
        {
            name: "start checkout",
            act: { click: "#checkout" },
            ready: `!document.querySelector("#checkout-form").hidden`,
            observed: ["Full name", "I accept the terms"],
        },
        {
            name: "enter name",
            act: { fill: "#name", text: customer.name },
            ready:
                `document.querySelector("#name").value === "${customer.name}"`,
        },
        {
            name: "enter email",
            act: { fill: "#email", text: customer.email },
            ready:
                `document.querySelector("#email").value === "${customer.email}"`,
        },
        {
            name: "accept terms",
            act: { click: "#terms" },
            ready: `document.querySelector("#terms").checked`,
        },
        {
            name: "place order",
            act: { click: "#place-order" },
            ready:
                `document.querySelector("#confirmation").textContent.includes("confirmed")`,
            observed: ["Order ORD-1001 confirmed"],
        },
    ],
    outcome: {
        source: "server-request-log",
        failure(requests) {
            const order = soleRequest(requests, "POST", "/api/orders");
            if (typeof order === "string") return order;
            const expected = {
                items: [{ id: cobalt_notebook.id, quantity: 1 }],
                ...customer,
                accepted_terms: true,
            };
            return sameJson(order.body, expected)
                ? null
                : `order was ${JSON.stringify(order.body)}`;
        },
    },
};

const docs_lookup: Task = {
    id: "docs-lookup",
    content: "pinned-fixture",
    steps: [
        {
            name: "open docs",
            act: { goto: `${fixture_origin}/docs/` },
            ready: `document.querySelectorAll("nav a").length === 6`,
            observed: ["Rate limits", "Webhooks"],
        },
        {
            name: "open rate limits",
            act: { click: `nav a[href="/docs/rate-limits"]` },
            ready: `document.title === "Rate limits"`,
            observed: ["Sustained limit", "600 requests per minute"],
        },
        {
            name: "expand burst limits",
            act: { click: "#burst summary" },
            ready: `document.querySelector("#burst").open`,
            observed: ["Burst allowance is 240 requests per 10 seconds."],
        },
        {
            name: "scroll to lazy changelog",
            act: { click: `a[href="#changelog"]` },
            ready:
                `document.querySelectorAll("#changelog-entries li").length === 5`,
            observed: ["Deprecated per-key hourly quotas"],
        },
    ],
    outcome: {
        source: "server-request-log",
        failure(requests) {
            for (const path of ["/docs/rate-limits", "/api/changelog"]) {
                const request = soleRequest(requests, "GET", path);
                if (typeof request === "string") return request;
            }
            return null;
        },
    },
};

const dashboard_ticket: Task = {
    id: "dashboard-ticket",
    content: "pinned-fixture",
    steps: [
        {
            name: "open dashboard",
            act: { goto: `${fixture_origin}/dashboard/` },
            ready:
                `document.querySelectorAll("#accounts tbody tr").length === 50`,
            observed: ["Account 0001", "Revenue"],
        },
        {
            name: "sort by revenue",
            act: { click: `th[data-key="revenue"]` },
            ready:
                `document.querySelector('th[data-key="revenue"]').getAttribute("aria-sort") === "descending"`,
            observed: [highest_revenue_account.name],
        },
        {
            name: "open top account",
            act: { click: "#accounts tbody tr:first-child" },
            ready: `document.querySelector("#detail").open`,
            observed: [highest_revenue_account.name, "File ticket"],
        },
        {
            name: "write note",
            act: { fill: "#note", text: ticket_note },
            ready: `document.querySelector("#note").value === "${ticket_note}"`,
        },
        {
            name: "submit ticket",
            act: { click: "#submit-ticket" },
            ready:
                `document.querySelector("#ticket-status").textContent.includes("filed")`,
            observed: [
                `Ticket TCK-1 filed for ${highest_revenue_account.name}`,
            ],
        },
    ],
    outcome: {
        source: "server-request-log",
        failure(requests) {
            const ticket = soleRequest(requests, "POST", "/api/tickets");
            if (typeof ticket === "string") return ticket;
            const expected = {
                account_id: highest_revenue_account.id,
                note: ticket_note,
            };
            return sameJson(ticket.body, expected)
                ? null
                : `ticket was ${JSON.stringify(ticket.body)}`;
        },
    },
};

const wikipedia_search: Task = {
    id: "live-wikipedia-search",
    content: "live-unpinned",
    steps: [
        {
            name: "open main page",
            act: { goto: "https://en.wikipedia.org/wiki/Main_Page" },
            ready: `document.querySelector("input[name=search]") !== null`,
            observed: ["Wikipedia"],
        },
        {
            name: "type query",
            act: { fill: "input[name=search]", text: "Firecracker (software)" },
            ready:
                `document.querySelector("input[name=search]").value === "Firecracker (software)"`,
        },
        {
            name: "submit search",
            act: { press: "Enter" },
            ready: `location.pathname === "/wiki/Firecracker_(software)"`,
            observed: ["Amazon Web Services"],
        },
    ],
    outcome: {
        source: "page-state",
        holds:
            `document.querySelector("#firstHeading").textContent.includes("Firecracker")`,
    },
};

const hacker_news_paging: Task = {
    id: "live-hacker-news-paging",
    content: "live-unpinned",
    steps: [
        {
            name: "open front page",
            act: { goto: "https://news.ycombinator.com/" },
            ready: `document.querySelectorAll(".athing").length === 30`,
            observed: ["Hacker News"],
        },
        {
            name: "next page",
            act: { click: "a.morelink" },
            ready: `location.search.includes("p=2")`,
            observed: ["31."],
        },
    ],
    outcome: {
        source: "page-state",
        holds: `document.querySelectorAll(".athing").length === 30`,
    },
};

const mdn_navigation: Task = {
    id: "live-mdn-navigation",
    content: "live-unpinned",
    steps: [
        {
            name: "open fetch api",
            act: {
                goto:
                    "https://developer.mozilla.org/en-US/docs/Web/API/Fetch_API",
            },
            ready: `document.title.includes("Fetch API")`,
            observed: ["Fetch API"],
        },
        {
            name: "open using fetch",
            act: { click: `main a[href$="/Fetch_API/Using_Fetch"]` },
            ready: `location.pathname.endsWith("/Fetch_API/Using_Fetch")`,
            observed: ["Using the Fetch API"],
        },
    ],
    outcome: {
        source: "page-state",
        holds:
            `document.querySelector("h1").textContent === "Using the Fetch API"`,
    },
};

export const tasks: Task[] = [
    shop_checkout,
    docs_lookup,
    dashboard_ticket,
    wikipedia_search,
    hacker_news_paging,
    mdn_navigation,
];
