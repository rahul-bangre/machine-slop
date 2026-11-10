import {
    errorResponse,
    firstDifference,
    formatMoney,
    htmlResponse,
    isoDate,
    monthName,
    notFound,
    pause,
    readObject,
    renderPage,
    routeOf,
    seededRandom,
    settledSite,
    shortDate,
    type Workflow,
    type WorkflowSite,
} from "./workflow.ts";

export type InvoiceStatus = "paid" | "unpaid" | "disputed";

export type Invoice = {
    id: string;
    customer: string;
    issued_on: string;
    amount_cents: number;
    status: InvoiceStatus;
    dispute_reason: string | null;
};

export type InvoiceDisputeScenario = {
    invoices: Invoice[];
    customer: string;
    month: number;
    reason: string;
    target_id: string;
};

export const dispute_reasons: Record<string, string> = {
    "duplicate-charge": "Duplicate charge",
    "incorrect-amount": "Incorrect amount",
    "service-not-received": "Service not received",
    "not-authorised": "Not authorised",
};

const customer_pool = [
    "Halvorsen Freight",
    "Brightwater Dental",
    "Okafor & Lowe",
    "Mistral Joinery",
    "Pemberton Labs",
    "Quayside Bakery",
    "Northgate Clinics",
    "Saffron Mill Studio",
];

const status_labels: Record<InvoiceStatus, string> = {
    paid: "Paid",
    unpaid: "Unpaid",
    disputed: "Disputed",
};

const page_size = 8;
const list_latency_ms = 300;
const dispute_latency_ms = 150;
const year = 2026;

export function invoiceDisputeScenario(seed: number): InvoiceDisputeScenario {
    const random = seededRandom(seed);
    const customers = random.sample(customer_pool, 6);
    const customer = customers[0]!;
    const month = random.int(1, 6);
    const other_month = ((month + random.int(0, 4)) % 6) + 1;
    const reason_keys = Object.keys(dispute_reasons);
    const reason = random.pick(reason_keys);
    const target_amount = random.int(3_000, 7_000) * 100 + random.int(1, 99);

    const drafts: Omit<Invoice, "id">[] = [];
    const add = (
        owner: string,
        issued_month: number,
        amount_cents: number,
        status: InvoiceStatus,
    ) => {
        const draft = {
            customer: owner,
            issued_on: isoDate(year, issued_month, random.int(1, 28)),
            amount_cents,
            status,
            dispute_reason: status === "disputed"
                ? random.pick(reason_keys)
                : null,
        };
        drafts.push(draft);
        return draft;
    };

    const target = add(customer, month, target_amount, "unpaid");
    const above = () => target_amount + random.int(150, 2_500) * 100;
    add(
        customer,
        month,
        target_amount - random.int(100, 1_500) * 100,
        "unpaid",
    );
    add(customer, month, above(), "paid");
    add(customer, month, above(), "disputed");
    add(customer, other_month, above(), "unpaid");
    add(random.pick(customers.slice(1)), month, above(), "unpaid");
    for (let index = 0; index < 30; index++) {
        let owner = random.pick(customers);
        let issued_month = random.int(1, 6);
        while (owner === customer && issued_month === month) {
            owner = random.pick(customers);
            issued_month = random.int(1, 6);
        }
        const roll = random.next();
        const status = roll < 0.5
            ? "paid"
            : roll < 0.88
            ? "unpaid"
            : "disputed";
        add(
            owner,
            issued_month,
            random.int(150, 9_500) * 100 + random.int(0, 99),
            status,
        );
    }

    const ordered = random.shuffle(drafts).sort((left, right) =>
        right.issued_on.localeCompare(left.issued_on)
    );
    const invoices = ordered.map((draft, index) => ({
        id: `INV-${3100 + ordered.length - index}`,
        ...draft,
    }));
    const target_id = invoices[ordered.indexOf(target)]!.id;
    return { invoices, customer, month, reason, target_id };
}

function invoiceView(invoice: Invoice) {
    return {
        id: invoice.id,
        customer: invoice.customer,
        issued_on: invoice.issued_on,
        issued_label: shortDate(invoice.issued_on),
        amount_cents: invoice.amount_cents,
        amount_label: formatMoney(invoice.amount_cents, "GBP"),
        status: invoice.status,
        status_label: status_labels[invoice.status],
    };
}

const reason_options = Object.entries(dispute_reasons).map(([value, label]) =>
    `<label><input type="radio" name="reason" value="${value}"> ${label}</label>`
).join("\n");

const page = renderPage({
    title: "Invoices",
    brand: "Ledgerly · Accounts receivable",
    style: `
  td.amount { font-variant-numeric: tabular-nums; }
  #more { margin-top: 16px; }
`,
    body: `
<main>
  <h1 id="heading">Invoices</h1>
  <p id="summary" class="hint"></p>
  <p id="notice" class="notice" role="status" aria-live="polite"></p>
  <table aria-labelledby="heading" id="table" aria-busy="true">
    <thead>
      <tr>
        <th scope="col">Invoice</th>
        <th scope="col">Customer</th>
        <th scope="col">Issued</th>
        <th scope="col">Amount</th>
        <th scope="col">Status</th>
        <th scope="col"><span class="visually-hidden">Actions</span></th>
      </tr>
    </thead>
    <tbody id="rows"></tbody>
  </table>
  <p id="loading" role="status">Loading invoices…</p>
  <button id="more" type="button" hidden>Load more invoices</button>
</main>
<dialog id="dispute" aria-labelledby="dispute-title">
  <form id="dispute-form">
    <h2 id="dispute-title">Dispute invoice</h2>
    <p id="dispute-summary"></p>
    <fieldset>
      <legend>Reason for dispute</legend>
      ${reason_options}
    </fieldset>
    <p id="dispute-error" class="error" role="alert"></p>
    <div class="actions">
      <button type="button" id="dispute-cancel">Cancel</button>
      <button type="submit" class="primary" id="dispute-submit">Submit dispute</button>
    </div>
  </form>
</dialog>
`,
    script: `
let next_offset = 0;
let current_invoice = null;
function renderRow(invoice) {
    const action = invoice.status === "unpaid"
        ? h("button", { type: "button", "aria-label": "Dispute " + invoice.id, onclick: () => openDispute(invoice) }, "Dispute")
        : "";
    return h("tr", { id: "row-" + invoice.id },
        h("th", { scope: "row" }, invoice.id),
        h("td", {}, invoice.customer),
        h("td", {}, invoice.issued_label),
        h("td", { class: "amount" }, invoice.amount_label),
        h("td", {}, invoice.status_label),
        h("td", {}, action));
}
async function loadMore() {
    byId("more").hidden = true;
    byId("loading").hidden = false;
    byId("table").setAttribute("aria-busy", "true");
    const result = await api("GET", "/api/invoices?offset=" + next_offset);
    byId("loading").hidden = true;
    byId("table").setAttribute("aria-busy", "false");
    if (!result.ok) {
        byId("notice").textContent = "Invoices could not be loaded.";
        return;
    }
    for (const invoice of result.data.invoices) byId("rows").append(renderRow(invoice));
    next_offset = result.data.next_offset;
    byId("summary").textContent = "Showing " + byId("rows").children.length + " of " + result.data.total + " invoices, newest first.";
    byId("more").hidden = next_offset === null;
}
function openDispute(invoice) {
    current_invoice = invoice;
    byId("dispute-form").reset();
    byId("dispute-title").textContent = "Dispute " + invoice.id;
    byId("dispute-summary").textContent = invoice.customer + ", issued " + invoice.issued_label + ", " + invoice.amount_label + ".";
    byId("dispute-error").textContent = "";
    byId("dispute").showModal();
}
byId("dispute-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const reason = new FormData(byId("dispute-form")).get("reason");
    if (!reason) {
        byId("dispute-error").textContent = "Choose a reason for the dispute.";
        return;
    }
    byId("dispute-submit").disabled = true;
    const result = await api("POST", "/api/invoices/" + encodeURIComponent(current_invoice.id) + "/dispute", { reason });
    byId("dispute-submit").disabled = false;
    if (!result.ok) {
        byId("dispute-error").textContent = (result.data && result.data.error) || "The dispute could not be submitted.";
        return;
    }
    byId("dispute").close();
    byId("row-" + current_invoice.id).replaceWith(renderRow(result.data));
    byId("notice").textContent = current_invoice.id + " is now disputed.";
});
byId("dispute-cancel").addEventListener("click", () => byId("dispute").close());
byId("more").addEventListener("click", loadMore);
loadMore();
`,
});

function openSite(scenario: InvoiceDisputeScenario): WorkflowSite {
    const invoices = scenario.invoices.map((invoice) => ({ ...invoice }));
    const committed = (invoice: Invoice) => ({
        status: invoice.status,
        dispute_reason: invoice.dispute_reason,
    });
    const expected = new Map(
        scenario.invoices.map((invoice) => [
            invoice.id,
            invoice.id === scenario.target_id
                ? { status: "disputed", dispute_reason: scenario.reason }
                : committed(invoice),
        ]),
    );

    return settledSite({
        async handle(request) {
            const { method, path, query } = routeOf(request);
            if (method === "GET" && path === "/") return htmlResponse(page);
            if (method === "GET" && path === "/api/invoices") {
                await pause(list_latency_ms);
                const offset = Number(query.get("offset") ?? "0");
                if (!Number.isInteger(offset) || offset < 0) {
                    return errorResponse(400, "Invalid offset");
                }
                const end = offset + page_size;
                return Response.json({
                    invoices: invoices.slice(offset, end).map(invoiceView),
                    next_offset: end < invoices.length ? end : null,
                    total: invoices.length,
                });
            }
            const dispute = path.match(/^\/api\/invoices\/([^/]+)\/dispute$/);
            if (method === "POST" && dispute) {
                await pause(dispute_latency_ms);
                const invoice = invoices.find((entry) =>
                    entry.id === decodeURIComponent(dispute[1]!)
                );
                if (!invoice) return notFound();
                const body = await readObject(request);
                const reason = body?.reason;
                if (
                    typeof reason !== "string" || !(reason in dispute_reasons)
                ) {
                    return errorResponse(400, "Choose a valid reason.", {
                        reason: "invalid",
                    });
                }
                if (invoice.status !== "unpaid") {
                    return errorResponse(
                        409,
                        `${invoice.id} is ${
                            status_labels[invoice.status].toLowerCase()
                        } and cannot be disputed.`,
                    );
                }
                invoice.status = "disputed";
                invoice.dispute_reason = reason;
                return Response.json(invoiceView(invoice));
            }
            return notFound();
        },
        outcome() {
            const newly_disputed = invoices.filter((invoice, index) =>
                invoice.status === "disputed" &&
                scenario.invoices[index]!.status !== "disputed"
            );
            if (newly_disputed.length === 0) return "no invoice was disputed";
            return firstDifference(
                "invoice",
                expected,
                new Map(invoices.map((invoice) => [
                    invoice.id,
                    committed(invoice),
                ])),
            );
        },
    });
}

export const invoice_dispute: Workflow = {
    id: "invoice-dispute",
    covers: ["async-loading", "dialogs"],
    seeds: [13, 15, 20],
    open(seed) {
        const scenario = invoiceDisputeScenario(seed);
        const prompt =
            `Please dispute the largest unpaid invoice from ${scenario.customer} that was issued in ${
                monthName(scenario.month)
            } ${year}. The reason for the dispute is "${
                dispute_reasons[scenario.reason]
            }".`;
        return { prompt, site: openSite(scenario) };
    },
};
