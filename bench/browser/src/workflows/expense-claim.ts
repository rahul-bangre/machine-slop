import {
    errorResponse,
    firstDifference,
    formatMoney,
    htmlResponse,
    isIsoDate,
    isoDate,
    longDate,
    notFound,
    pause,
    readObject,
    renderPage,
    routeOf,
    seededRandom,
    settledSite,
    shiftDate,
    type Workflow,
    type WorkflowSite,
} from "./workflow.ts";

export type CostCentre = { id: string; code: string; name: string };

export type ClaimCategory = "travel" | "meals";

export type Claim = {
    category: ClaimCategory;
    merchant: string;
    expense_date: string;
    purpose: string;
    amount_cents: number;
    currency: string;
    trip_start: string | null;
    trip_end: string | null;
    attendees: number | null;
    cost_centre_id: string;
};

export type ExpenseClaimScenario = {
    cost_centres: CostCentre[];
    expected: Omit<Claim, "purpose">;
    purpose_keyword: string;
    purpose: string;
    item: string;
    department: string;
    region: string;
};

const departments = [
    "Field Sales",
    "Inside Sales",
    "Customer Success",
    "Platform Engineering",
    "Marketing",
    "Finance",
];
const regions = ["Nordics", "Baltics", "Iberia", "Benelux"];
const currencies = ["EUR", "GBP", "SEK"];
const travel_merchants: readonly (readonly [string, string])[] = [
    ["Deutsche Bahn", "a train ticket"],
    ["SAS", "a return flight"],
    ["Eurostar", "a train ticket"],
    ["Scandic Hotels", "a hotel stay"],
];
const meal_merchants: readonly (readonly [string, string])[] = [
    ["Trattoria Romana", "a team dinner"],
    ["Café Nord", "a client lunch"],
    ["Sushi Kaito", "a client dinner"],
];
const clients = ["Lindqvist", "Marlowe", "Achterberg", "Ferreira", "Kowalczyk"];
const occasions = [
    "onboarding workshop",
    "quarterly review",
    "renewal meeting",
];

const today = "2026-05-04";
const earliest = "2026-01-01";
const search_latency_ms = 250;
const submit_latency_ms = 200;

export function expenseClaimScenario(seed: number): ExpenseClaimScenario {
    const random = seededRandom(seed);
    const codes = random.shuffle(
        Array.from(
            { length: departments.length * regions.length },
            (_, index) => 4100 + index * 3,
        ),
    );
    const cost_centres = departments.flatMap((department, row) =>
        regions.map((region, column) => {
            const code = `CC-${codes[row * regions.length + column]}`;
            return {
                id: code.toLowerCase(),
                code,
                name: `${department} – ${region}`,
            };
        })
    );
    const department = random.pick(departments.slice(0, 4));
    const region = random.pick(regions);
    const cost_centre = cost_centres.find((entry) =>
        entry.name === `${department} – ${region}`
    )!;

    const category: ClaimCategory = random.next() < 0.5 ? "travel" : "meals";
    const [merchant, item] = random.pick(
        category === "travel" ? travel_merchants : meal_merchants,
    );
    const expense_date = isoDate(2026, random.int(2, 4), random.int(3, 26));
    const purpose_keyword = random.pick(clients);
    const purpose = `the ${purpose_keyword} ${random.pick(occasions)}`;
    const currency = random.pick(currencies);
    const amount_cents = random.int(25, 900) * 100 + random.int(1, 99);
    const trip_start = category === "travel"
        ? shiftDate(expense_date, -random.int(0, 2))
        : null;
    const trip_end = category === "travel"
        ? shiftDate(expense_date, random.int(1, 3))
        : null;
    const attendees = category === "meals" ? random.int(2, 9) : null;
    return {
        cost_centres,
        expected: {
            category,
            merchant,
            expense_date,
            amount_cents,
            currency,
            trip_start,
            trip_end,
            attendees,
            cost_centre_id: cost_centre.id,
        },
        purpose_keyword,
        purpose,
        item,
        department,
        region,
    };
}

const page = renderPage({
    title: "New expense claim",
    brand: "Tallyhouse · Expenses",
    style: `
  main { max-width: 640px; }
  #progress { display: flex; gap: 12px; list-style: none; padding: 0; color: #5b6470; }
  #progress li[aria-current=step] { color: #1d232a; font-weight: 700; }
  section.step { background: #fff; border: 1px solid #dde1e6; border-radius: 8px; padding: 16px 20px; }
  #cost-centre-options { list-style: none; margin: 4px 0 0; padding: 0; border: 1px solid #dde1e6; border-radius: 4px; background: #fff; }
  #cost-centre-options li { padding: 6px 8px; cursor: pointer; }
  #cost-centre-options li:hover { background: #eef3fb; }
  dl { display: grid; grid-template-columns: max-content 1fr; gap: 6px 16px; }
  dt { font-weight: 600; }
  dd { margin: 0; }
`,
    body: `
<main>
  <h1>New expense claim</h1>
  <ol id="progress" aria-label="Progress">
    <li data-step="1">1. Expense</li>
    <li data-step="2">2. Amount</li>
    <li data-step="3">3. Cost centre</li>
    <li data-step="4">4. Review</li>
  </ol>
  <form id="claim" novalidate>
    <section class="step" data-step="1" aria-labelledby="step-1-title">
      <h2 id="step-1-title">Expense</h2>
      <fieldset>
        <legend>Category</legend>
        <label><input type="radio" name="category" value="travel"> Travel</label>
        <label><input type="radio" name="category" value="meals"> Meals and entertainment</label>
      </fieldset>
      <label for="merchant">Merchant</label>
      <input type="text" id="merchant" name="merchant" autocomplete="off">
      <label for="expense-date">Date of expense</label>
      <input type="text" id="expense-date" name="expense_date" placeholder="YYYY-MM-DD" aria-describedby="expense-date-hint" autocomplete="off">
      <p id="expense-date-hint" class="hint">Format YYYY-MM-DD, for example 2026-03-09.</p>
      <label for="purpose">Business purpose</label>
      <input type="text" id="purpose" name="purpose" autocomplete="off">
    </section>
    <section class="step" data-step="2" aria-labelledby="step-2-title" hidden>
      <h2 id="step-2-title">Amount</h2>
      <label for="amount">Amount on the receipt</label>
      <input type="text" id="amount" name="amount" inputmode="decimal" placeholder="0.00" autocomplete="off">
      <fieldset>
        <legend>Currency</legend>
        <label><input type="radio" name="currency" value="EUR"> EUR (€)</label>
        <label><input type="radio" name="currency" value="GBP"> GBP (£)</label>
        <label><input type="radio" name="currency" value="SEK"> SEK (kr)</label>
      </fieldset>
      <div id="travel-section" hidden>
        <h3>Trip</h3>
        <label for="trip-start">Trip start date</label>
        <input type="text" id="trip-start" name="trip_start" placeholder="YYYY-MM-DD" autocomplete="off">
        <label for="trip-end">Trip end date</label>
        <input type="text" id="trip-end" name="trip_end" placeholder="YYYY-MM-DD" autocomplete="off">
      </div>
      <div id="meals-section" hidden>
        <h3>Attendees</h3>
        <label for="attendees">Number of attendees, including you</label>
        <input type="text" id="attendees" name="attendees" inputmode="numeric" autocomplete="off">
      </div>
    </section>
    <section class="step" data-step="3" aria-labelledby="step-3-title" hidden>
      <h2 id="step-3-title">Cost centre</h2>
      <label for="cost-centre-search">Search cost centres</label>
      <input type="text" id="cost-centre-search" role="combobox" aria-autocomplete="list" aria-expanded="false" aria-controls="cost-centre-options" autocomplete="off" placeholder="Name or code">
      <p id="cost-centre-status" class="hint" role="status" aria-live="polite"></p>
      <ul id="cost-centre-options" role="listbox" aria-label="Matching cost centres" hidden></ul>
      <p id="cost-centre-selected"></p>
    </section>
    <section class="step" data-step="4" aria-labelledby="step-4-title" hidden>
      <h2 id="step-4-title">Review</h2>
      <dl id="review"></dl>
    </section>
    <div id="errors" class="error" role="alert"></div>
    <div class="actions">
      <button type="button" id="back" hidden>Back</button>
      <button type="button" id="next" class="primary">Continue</button>
      <button type="submit" id="submit" class="primary" hidden>Submit claim</button>
    </div>
  </form>
  <section id="done" hidden aria-labelledby="done-title">
    <h2 id="done-title">Claim submitted</h2>
    <p id="done-text"></p>
  </section>
</main>
`,
    script: `
let step = 1;
let cost_centre = null;
let search_sequence = 0;
let search_timer = null;
const field_labels = { category: "Category", merchant: "Merchant", expense_date: "Date of expense", purpose: "Business purpose", amount: "Amount", currency: "Currency", trip_start: "Trip start date", trip_end: "Trip end date", attendees: "Attendees", cost_centre_id: "Cost centre" };
function value(name) {
    const entry = new FormData(byId("claim")).get(name);
    return entry === null ? "" : String(entry).trim();
}
function isDate(text) {
    if (text.length !== 10 || text[4] !== "-" || text[7] !== "-") return false;
    const date = new Date(text + "T00:00:00Z");
    return !Number.isNaN(date.getTime()) && date.toISOString().slice(0, 10) === text;
}
function stepErrors(number) {
    const errors = [];
    if (number === 1) {
        if (!value("category")) errors.push("Choose a category.");
        if (!value("merchant")) errors.push("Enter the merchant.");
        if (!isDate(value("expense_date"))) errors.push("Enter the date of expense as YYYY-MM-DD.");
        if (!value("purpose")) errors.push("Enter the business purpose.");
    }
    if (number === 2) {
        const amount = Number(value("amount"));
        if (!value("amount") || !(amount > 0)) errors.push("Enter the amount as a number, for example 84.60.");
        if (!value("currency")) errors.push("Choose a currency.");
        if (value("category") === "travel") {
            if (!isDate(value("trip_start"))) errors.push("Enter the trip start date as YYYY-MM-DD.");
            if (!isDate(value("trip_end"))) errors.push("Enter the trip end date as YYYY-MM-DD.");
        }
        if (value("category") === "meals" && !(Number.isInteger(Number(value("attendees"))) && Number(value("attendees")) > 0)) {
            errors.push("Enter the number of attendees.");
        }
    }
    if (number === 3 && !cost_centre) errors.push("Choose a cost centre from the search results.");
    return errors;
}
function showErrors(errors) {
    byId("errors").replaceChildren(...(errors.length ? [h("ul", {}, errors.map((error) => h("li", {}, error)))] : []));
}
function claimBody() {
    const category = value("category");
    return {
        category,
        merchant: value("merchant"),
        expense_date: value("expense_date"),
        purpose: value("purpose"),
        amount: value("amount"),
        currency: value("currency"),
        trip_start: category === "travel" ? value("trip_start") : null,
        trip_end: category === "travel" ? value("trip_end") : null,
        attendees: category === "meals" ? Number(value("attendees")) : null,
        cost_centre_id: cost_centre ? cost_centre.id : null,
    };
}
function renderReview() {
    const body = claimBody();
    const rows = [
        ["Category", body.category === "travel" ? "Travel" : "Meals and entertainment"],
        ["Merchant", body.merchant],
        ["Date of expense", body.expense_date],
        ["Business purpose", body.purpose],
        ["Amount", body.amount + " " + body.currency],
    ];
    if (body.category === "travel") rows.push(["Trip", body.trip_start + " to " + body.trip_end]);
    if (body.category === "meals") rows.push(["Attendees", String(body.attendees)]);
    rows.push(["Cost centre", cost_centre.code + " " + cost_centre.name]);
    byId("review").replaceChildren(...rows.flatMap(([term, detail]) => [h("dt", {}, term), h("dd", {}, detail)]));
}
function showStep(number) {
    step = number;
    for (const section of document.querySelectorAll("section.step")) section.hidden = Number(section.dataset.step) !== number;
    for (const item of document.querySelectorAll("#progress li")) {
        if (Number(item.dataset.step) === number) item.setAttribute("aria-current", "step");
        else item.removeAttribute("aria-current");
    }
    byId("travel-section").hidden = value("category") !== "travel";
    byId("meals-section").hidden = value("category") !== "meals";
    byId("back").hidden = number === 1;
    byId("next").hidden = number === 4;
    byId("submit").hidden = number !== 4;
    if (number === 4) renderReview();
    showErrors([]);
}
function chooseCostCentre(entry) {
    cost_centre = entry;
    byId("cost-centre-selected").textContent = "Selected cost centre: " + entry.code + " " + entry.name;
    byId("cost-centre-options").hidden = true;
    byId("cost-centre-search").setAttribute("aria-expanded", "false");
}
async function searchCostCentres() {
    const query = byId("cost-centre-search").value.trim();
    const sequence = ++search_sequence;
    if (query.length < 2) {
        byId("cost-centre-options").hidden = true;
        byId("cost-centre-status").textContent = "Type at least two characters.";
        return;
    }
    byId("cost-centre-status").textContent = "Searching…";
    const result = await api("GET", "/api/cost-centres?q=" + encodeURIComponent(query));
    if (sequence !== search_sequence) return;
    const matches = result.ok ? result.data.cost_centres : [];
    byId("cost-centre-status").textContent = matches.length + " matching cost centre" + (matches.length === 1 ? "" : "s") + ".";
    byId("cost-centre-options").replaceChildren(...matches.map((entry) =>
        h("li", { role: "option", id: "option-" + entry.id, tabindex: "-1", "aria-selected": cost_centre && cost_centre.id === entry.id ? "true" : "false", onclick: () => chooseCostCentre(entry) }, entry.code + " " + entry.name)));
    byId("cost-centre-options").hidden = matches.length === 0;
    byId("cost-centre-search").setAttribute("aria-expanded", matches.length > 0 ? "true" : "false");
}
byId("cost-centre-search").addEventListener("input", () => {
    clearTimeout(search_timer);
    search_timer = setTimeout(searchCostCentres, 150);
});
for (const radio of document.querySelectorAll("input[name=category]")) radio.addEventListener("change", () => showStep(step));
byId("next").addEventListener("click", () => {
    const errors = stepErrors(step);
    if (errors.length) {
        showErrors(errors);
        return;
    }
    showStep(step + 1);
});
byId("back").addEventListener("click", () => showStep(step - 1));
byId("claim").addEventListener("submit", async (event) => {
    event.preventDefault();
    if (step !== 4) return;
    byId("submit").disabled = true;
    const result = await api("POST", "/api/claims", claimBody());
    byId("submit").disabled = false;
    if (!result.ok) {
        const fields = (result.data && result.data.fields) || {};
        const messages = Object.entries(fields).map(([name, message]) => (field_labels[name] || name) + ": " + message);
        showErrors(messages.length ? messages : [(result.data && result.data.error) || "The claim could not be submitted."]);
        return;
    }
    byId("claim").hidden = true;
    byId("progress").hidden = true;
    byId("done-text").textContent = "Claim " + result.data.id + " was submitted for approval.";
    byId("done").hidden = false;
});
showStep(1);
`,
});

function validateClaim(
    body: Record<string, unknown>,
    cost_centres: readonly CostCentre[],
): { claim: Claim } | { fields: Record<string, string> } {
    const fields: Record<string, string> = {};
    const text = (name: string) => {
        const entry = body[name];
        return typeof entry === "string" ? entry.trim() : "";
    };
    const category = text("category");
    if (category !== "travel" && category !== "meals") {
        fields.category = "must be travel or meals";
    }
    const merchant = text("merchant");
    if (!merchant || merchant.length > 80) {
        fields.merchant = "is required (80 characters at most)";
    }
    const purpose = text("purpose");
    if (!purpose || purpose.length > 200) {
        fields.purpose = "is required (200 characters at most)";
    }
    const expense_date = text("expense_date");
    if (!isIsoDate(expense_date)) {
        fields.expense_date = "must be a date written as YYYY-MM-DD";
    } else if (expense_date > today || expense_date < earliest) {
        fields.expense_date = `must be between ${earliest} and ${today}`;
    }
    const amount = text("amount");
    const amount_cents = /^\d{1,6}(\.\d{1,2})?$/.test(amount)
        ? Math.round(Number(amount) * 100)
        : 0;
    if (amount_cents <= 0) {
        fields.amount = "must be a positive amount with at most two decimals";
    }
    const currency = text("currency");
    if (!currencies.includes(currency)) {
        fields.currency = `must be one of ${currencies.join(", ")}`;
    }
    let trip_start: string | null = null;
    let trip_end: string | null = null;
    let attendees: number | null = null;
    if (category === "travel") {
        trip_start = text("trip_start");
        trip_end = text("trip_end");
        if (!isIsoDate(trip_start)) {
            fields.trip_start = "is required for travel";
        }
        if (!isIsoDate(trip_end)) fields.trip_end = "is required for travel";
        if (isIsoDate(trip_start) && isIsoDate(trip_end)) {
            if (trip_start > trip_end) {
                fields.trip_end = "must not be before the trip start date";
            } else if (
                isIsoDate(expense_date) &&
                (expense_date < trip_start || expense_date > trip_end)
            ) {
                fields.expense_date = "must fall within the trip";
            }
        }
        if (body.attendees != null) {
            fields.attendees = "only applies to meals";
        }
    }
    if (category === "meals") {
        const count = body.attendees;
        if (
            typeof count !== "number" || !Number.isInteger(count) ||
            count < 1 || count > 50
        ) {
            fields.attendees = "must be a whole number from 1 to 50";
        } else {
            attendees = count;
        }
        if (body.trip_start != null || body.trip_end != null) {
            fields.trip_start = "only applies to travel";
        }
    }
    const cost_centre = cost_centres.find((entry) =>
        entry.id === body.cost_centre_id
    );
    if (!cost_centre) fields.cost_centre_id = "must be an existing cost centre";
    if (Object.keys(fields).length > 0) return { fields };
    return {
        claim: {
            category: category as ClaimCategory,
            merchant,
            expense_date,
            purpose,
            amount_cents,
            currency,
            trip_start,
            trip_end,
            attendees,
            cost_centre_id: cost_centre!.id,
        },
    };
}

function openSite(scenario: ExpenseClaimScenario): WorkflowSite {
    const claims: (Claim & { id: string })[] = [];

    return settledSite({
        async handle(request) {
            const { method, path, query } = routeOf(request);
            if (method === "GET" && path === "/") return htmlResponse(page);
            if (method === "GET" && path === "/api/cost-centres") {
                await pause(search_latency_ms);
                const terms = (query.get("q") ?? "").toLowerCase().split(
                    /[\s–-]+/,
                ).filter((term) => term.length > 0);
                const matches = terms.length === 0
                    ? []
                    : scenario.cost_centres.filter((entry) => {
                        const haystack = `${entry.code} ${entry.name}`
                            .toLowerCase();
                        return terms.every((term) => haystack.includes(term));
                    }).slice(0, 6);
                return Response.json({ cost_centres: matches });
            }
            if (method === "POST" && path === "/api/claims") {
                await pause(submit_latency_ms);
                const body = await readObject(request);
                if (!body) return errorResponse(400, "Malformed request.");
                const validated = validateClaim(body, scenario.cost_centres);
                if ("fields" in validated) {
                    return errorResponse(
                        422,
                        "The claim has errors.",
                        validated.fields,
                    );
                }
                const id = `EXP-${1041 + claims.length}`;
                claims.push({ id, ...validated.claim });
                return Response.json({ id }, { status: 201 });
            }
            return notFound();
        },
        outcome() {
            if (claims.length === 0) return "no claim was submitted";
            if (claims.length > 1) {
                return `${claims.length} claims were submitted, expected one`;
            }
            const { id: _, purpose, ...claim } = claims[0]!;
            if (
                !purpose.toLowerCase().includes(
                    scenario.purpose_keyword.toLowerCase(),
                )
            ) {
                return `claim purpose "${purpose}" does not mention ${scenario.purpose_keyword}`;
            }
            const normalised = {
                ...claim,
                merchant: claim.merchant.toLowerCase(),
            };
            const expected = {
                ...scenario.expected,
                merchant: scenario.expected.merchant.toLowerCase(),
            };
            return firstDifference(
                "claim",
                new Map(Object.entries(expected)),
                new Map(Object.entries(normalised)),
            );
        },
    });
}

export const expense_claim: Workflow = {
    id: "expense-claim",
    covers: ["multi-step-form", "async-loading"],
    seeds: [34, 35, 38],
    open(seed) {
        const scenario = expenseClaimScenario(seed);
        const { expected } = scenario;
        const money = formatMoney(expected.amount_cents, expected.currency);
        const context = expected.category === "travel"
            ? `It was for ${scenario.purpose}, on a business trip that ran from ${
                longDate(expected.trip_start!)
            } to ${longDate(expected.trip_end!)}.`
            : `It was for ${scenario.purpose}, and there were ${expected.attendees} of us including me.`;
        const prompt =
            `Please file an expense claim for ${scenario.item} from ${expected.merchant} on ${
                longDate(expected.expense_date)
            }. The receipt total is ${money}. ${context} It should be charged to the ${scenario.department} cost centre for ${scenario.region}.`;
        return { prompt, site: openSite(scenario) };
    },
};
