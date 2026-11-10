import {
    errorResponse,
    firstDifference,
    htmlResponse,
    notFound,
    pause,
    readObject,
    renderPage,
    routeOf,
    seededRandom,
    settledSite,
    type Workflow,
    type WorkflowSite,
} from "./workflow.ts";

export type Address = {
    line1: string;
    line2: string;
    city: string;
    postcode: string;
    country: string;
};

export type Account = {
    name: string;
    email: string;
    password: string;
    shipping: Address;
    billing: Address;
};

export type AddressChangeScenario = { account: Account; new_shipping: Address };

const countries: Record<string, string> = {
    GB: "United Kingdom",
    DE: "Germany",
    NL: "Netherlands",
};

const address_pool: readonly Address[] = [
    {
        line1: "18 Larkspur Road",
        line2: "Flat 4",
        city: "Leeds",
        postcode: "LS6 2QT",
        country: "GB",
    },
    {
        line1: "7 Wharf Street",
        line2: "",
        city: "Bristol",
        postcode: "BS3 4HN",
        country: "GB",
    },
    {
        line1: "52 Kingsley Avenue",
        line2: "",
        city: "York",
        postcode: "YO10 5DD",
        country: "GB",
    },
    {
        line1: "Gneisenaustraße 41",
        line2: "Hinterhaus, 3. OG",
        city: "Berlin",
        postcode: "10961",
        country: "DE",
    },
    {
        line1: "Ottenser Hauptstraße 12",
        line2: "",
        city: "Hamburg",
        postcode: "22765",
        country: "DE",
    },
    {
        line1: "Karl-Liebknecht-Straße 88",
        line2: "",
        city: "Leipzig",
        postcode: "04275",
        country: "DE",
    },
    {
        line1: "Oudegracht 214",
        line2: "",
        city: "Utrecht",
        postcode: "3511 NR",
        country: "NL",
    },
    {
        line1: "Witte de Withstraat 33",
        line2: "2e verdieping",
        city: "Rotterdam",
        postcode: "3012 BM",
        country: "NL",
    },
];

const people = [
    "Elena Petrova",
    "Callum Fraser",
    "Noor Haddad",
    "Wim de Vries",
    "Ingrid Solberg",
];
const password_words = ["marigold", "lantern", "harbour", "juniper", "quartz"];
const api_latency_ms = 150;

export function addressChangeScenario(seed: number): AddressChangeScenario {
    const random = seededRandom(seed);
    const name = random.pick(people);
    const [shipping, billing, new_shipping] = random.sample(
        address_pool,
        3,
    ) as [
        Address,
        Address,
        Address,
    ];
    const password = `${random.pick(password_words)}-${
        random.pick(["Tram", "Ferry", "Kite", "Comet"])
    }-${random.int(1000, 9999)}`;
    return {
        account: {
            name,
            email: `${name.toLowerCase().replaceAll(" ", ".")}@mailbox.test`,
            password,
            shipping,
            billing,
        },
        new_shipping,
    };
}

export function normalisePostcode(
    country: string,
    postcode: string,
): string | null {
    const compact = postcode.toUpperCase().replace(/\s+/g, "");
    if (country === "GB") {
        return /^[A-Z]{1,2}\d[A-Z\d]?\d[A-Z]{2}$/.test(compact)
            ? `${compact.slice(0, -3)} ${compact.slice(-3)}`
            : null;
    }
    if (country === "DE") return /^\d{5}$/.test(compact) ? compact : null;
    if (country === "NL") {
        return /^\d{4}[A-Z]{2}$/.test(compact)
            ? `${compact.slice(0, 4)} ${compact.slice(4)}`
            : null;
    }
    return null;
}

function validateAddress(
    body: Record<string, unknown>,
): { address: Address } | { fields: Record<string, string> } {
    const text = (name: string) => {
        const entry = body[name];
        return typeof entry === "string"
            ? entry.trim().replace(/\s+/g, " ")
            : null;
    };
    const fields: Record<string, string> = {};
    const line1 = text("line1");
    const line2 = text("line2") ?? "";
    const city = text("city");
    const country = text("country") ?? "";
    const postcode = text("postcode");
    if (!line1 || line1.length > 80) fields.line1 = "is required";
    if (line2.length > 80) fields.line2 = "is too long";
    if (!city || city.length > 60) fields.city = "is required";
    if (!(country in countries)) fields.country = "must be a listed country";
    const normalised = postcode && country in countries
        ? normalisePostcode(country, postcode)
        : null;
    if (!normalised) {
        fields.postcode = `is not a valid postcode for ${
            countries[country] ?? "that country"
        }`;
    }
    if (Object.keys(fields).length > 0) return { fields };
    return {
        address: {
            line1: line1!,
            line2,
            city: city!,
            postcode: normalised!,
            country,
        },
    };
}

const address_style = `
  .card { background: #fff; border: 1px solid #dde1e6; border-radius: 8px; padding: 16px 20px; margin-bottom: 16px; }
  .card h2 { margin-top: 0; }
  address { font-style: normal; white-space: pre-line; }
  main { max-width: 640px; }
`;

const country_options = Object.entries(countries).map(([code, label]) =>
    `<label><input type="radio" name="country" value="${code}"> ${label}</label>`
).join("\n");

const account_page = renderPage({
    title: "Your account",
    brand: "Parcelpost · Account",
    style: address_style,
    body: `
<main>
  <h1>Your account</h1>
  <p id="loading" role="status">Loading your details…</p>
  <p id="notice" class="notice" role="status" aria-live="polite"></p>
  <section class="card" aria-labelledby="profile-title">
    <h2 id="profile-title">Profile</h2>
    <p id="profile"></p>
  </section>
  <section class="card" aria-labelledby="shipping-title">
    <h2 id="shipping-title">Shipping address</h2>
    <address id="shipping-current"></address>
    <p><button type="button" id="shipping-change" aria-label="Change shipping address">Change</button></p>
  </section>
  <section class="card" aria-labelledby="billing-title">
    <h2 id="billing-title">Billing address</h2>
    <address id="billing-current"></address>
    <p><button type="button" id="billing-change" aria-label="Change billing address">Change</button></p>
  </section>
  <section class="card" id="editor" aria-labelledby="editor-title" hidden>
    <h2 id="editor-title">New address</h2>
    <form id="address-form" novalidate>
      <label for="line1">Address line 1</label>
      <input type="text" id="line1" name="line1" autocomplete="off">
      <label for="line2">Address line 2 (optional)</label>
      <input type="text" id="line2" name="line2" autocomplete="off">
      <label for="city">Town or city</label>
      <input type="text" id="city" name="city" autocomplete="off">
      <label for="postcode">Postcode</label>
      <input type="text" id="postcode" name="postcode" autocomplete="off">
      <fieldset>
        <legend>Country</legend>
        ${country_options}
      </fieldset>
      <div id="address-errors" class="error" role="alert"></div>
      <div class="actions">
        <button type="button" id="address-cancel">Cancel</button>
        <button type="submit" class="primary" id="address-save">Save address</button>
      </div>
    </form>
  </section>
</main>
`,
    script: `
let editing = null;
const country_names = ${JSON.stringify(countries)};
function formatAddress(address) {
    return [address.line1, address.line2, address.city + " " + address.postcode, country_names[address.country]].filter(Boolean).join("\\n");
}
function expired() {
    location.href = "/sign-in?expired=1";
}
async function loadProfile() {
    const result = await api("GET", "/api/profile");
    if (result.status === 401) return expired();
    byId("loading").hidden = true;
    byId("profile").textContent = result.data.name + " · " + result.data.email;
    byId("shipping-current").textContent = formatAddress(result.data.shipping);
    byId("billing-current").textContent = formatAddress(result.data.billing);
}
function openEditor(kind) {
    editing = kind;
    byId("address-form").reset();
    byId("address-errors").replaceChildren();
    byId("editor-title").textContent = kind === "shipping" ? "New shipping address" : "New billing address";
    byId("editor").hidden = false;
    byId("line1").focus();
}
byId("shipping-change").addEventListener("click", () => openEditor("shipping"));
byId("billing-change").addEventListener("click", () => openEditor("billing"));
byId("address-cancel").addEventListener("click", () => {
    byId("editor").hidden = true;
});
byId("address-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const form = new FormData(byId("address-form"));
    const body = {};
    for (const name of ["line1", "line2", "city", "postcode", "country"]) body[name] = String(form.get(name) || "").trim();
    const missing = [];
    if (!body.line1) missing.push("Enter address line 1.");
    if (!body.city) missing.push("Enter the town or city.");
    if (!body.postcode) missing.push("Enter the postcode.");
    if (!body.country) missing.push("Choose a country.");
    if (missing.length) {
        byId("address-errors").replaceChildren(h("ul", {}, missing.map((message) => h("li", {}, message))));
        return;
    }
    byId("address-save").disabled = true;
    const result = await api("PUT", "/api/addresses/" + editing, body);
    byId("address-save").disabled = false;
    if (result.status === 401) return expired();
    if (!result.ok) {
        const fields = (result.data && result.data.fields) || {};
        const messages = Object.entries(fields).map(([name, message]) => name + " " + message);
        byId("address-errors").replaceChildren(h("ul", {}, (messages.length ? messages : ["The address could not be saved."]).map((message) => h("li", {}, message))));
        return;
    }
    byId("editor").hidden = true;
    byId("notice").textContent = (editing === "shipping" ? "Shipping" : "Billing") + " address updated.";
    await loadProfile();
});
loadProfile();
`,
});

const sign_in_page = renderPage({
    title: "Sign in",
    brand: "Parcelpost",
    style: address_style,
    body: `
<main>
  <h1>Sign in</h1>
  <p id="expired" class="error" role="alert" hidden>Your session has expired. Please sign in again.</p>
  <form id="sign-in" class="card" novalidate>
    <label for="email">Email address</label>
    <input type="email" id="email" name="email" autocomplete="off">
    <label for="password">Password</label>
    <input type="password" id="password" name="password" autocomplete="off">
    <p id="sign-in-error" class="error" role="alert"></p>
    <div class="actions"><button type="submit" class="primary" id="sign-in-submit">Sign in</button></div>
  </form>
</main>
`,
    script: `
byId("expired").hidden = new URLSearchParams(location.search).get("expired") !== "1";
byId("sign-in").addEventListener("submit", async (event) => {
    event.preventDefault();
    byId("sign-in-submit").disabled = true;
    const result = await api("POST", "/api/sign-in", { email: byId("email").value.trim(), password: byId("password").value });
    byId("sign-in-submit").disabled = false;
    if (!result.ok) {
        byId("sign-in-error").textContent = (result.data && result.data.error) || "Sign-in failed.";
        return;
    }
    location.href = "/";
});
`,
});

function comparable(address: Address) {
    return {
        street: `${address.line1} ${address.line2}`.toLowerCase().split(
            /[\s,]+/,
        ).filter((word) => word).sort().join(" "),
        city: address.city.toLowerCase(),
        postcode: address.postcode,
        country: address.country,
    };
}

function sessionToken(request: Request): string | null {
    const cookies = request.headers.get("cookie") ?? "";
    for (const part of cookies.split(";")) {
        const [name, value] = part.trim().split("=");
        if (name === "session" && value) return value;
    }
    return null;
}

function redirect(location: string): Response {
    return new Response(null, { status: 303, headers: { location } });
}

function openSite(scenario: AddressChangeScenario): WorkflowSite {
    const account = structuredClone(scenario.account);
    const sessions = new Map<
        string,
        { expires_on_save: boolean; expired: boolean }
    >();

    const sessionOf = (request: Request) => {
        const token = sessionToken(request);
        const session = token ? sessions.get(token) : undefined;
        if (!session) return "none" as const;
        return session.expired ? "expired" as const : session;
    };

    return settledSite({
        async handle(request) {
            const { method, path } = routeOf(request);
            if (method === "GET" && path === "/sign-in") {
                return htmlResponse(sign_in_page);
            }
            if (method === "POST" && path === "/api/sign-in") {
                await pause(api_latency_ms);
                const body = await readObject(request);
                if (!body) return errorResponse(400, "Malformed request.");
                const email = typeof body.email === "string"
                    ? body.email.trim().toLowerCase()
                    : "";
                if (
                    email !== account.email ||
                    body.password !== account.password
                ) {
                    return errorResponse(
                        401,
                        "That email address and password do not match.",
                    );
                }
                const token = `s${sessions.size + 1}`;
                sessions.set(token, {
                    expires_on_save: sessions.size === 0,
                    expired: false,
                });
                return Response.json({ signed_in: true }, {
                    headers: {
                        "set-cookie":
                            `session=${token}; Path=/; HttpOnly; SameSite=Lax`,
                    },
                });
            }
            const session = sessionOf(request);
            if (method === "GET" && path === "/") {
                if (session === "none") return redirect("/sign-in");
                if (session === "expired") {
                    return redirect("/sign-in?expired=1");
                }
                return htmlResponse(account_page);
            }
            if (!path.startsWith("/api/")) return notFound();
            await pause(api_latency_ms);
            if (typeof session === "string") {
                return errorResponse(401, "Your session has expired.");
            }
            if (method === "GET" && path === "/api/profile") {
                return Response.json({
                    name: account.name,
                    email: account.email,
                    shipping: account.shipping,
                    billing: account.billing,
                });
            }
            const kind = path.match(/^\/api\/addresses\/(shipping|billing)$/)
                ?.[1] as "shipping" | "billing" | undefined;
            if (method === "PUT" && kind) {
                if (session.expires_on_save) {
                    session.expired = true;
                    return errorResponse(401, "Your session has expired.");
                }
                const body = await readObject(request);
                if (!body) return errorResponse(400, "Malformed request.");
                const validated = validateAddress(body);
                if ("fields" in validated) {
                    return errorResponse(
                        422,
                        "The address has errors.",
                        validated.fields,
                    );
                }
                account[kind] = validated.address;
                return Response.json(validated.address);
            }
            return notFound();
        },
        outcome() {
            return firstDifference(
                "account",
                new Map(Object.entries({
                    shipping: comparable(scenario.new_shipping),
                    billing: comparable(scenario.account.billing),
                })),
                new Map(Object.entries({
                    shipping: comparable(account.shipping),
                    billing: comparable(account.billing),
                })),
            );
        },
    });
}

export const address_change: Workflow = {
    id: "address-change",
    covers: ["recovery"],
    seeds: [51, 52, 55],
    open(seed) {
        const scenario = addressChangeScenario(seed);
        const { account, new_shipping } = scenario;
        const lines = [
            new_shipping.line1,
            new_shipping.line2,
            `${new_shipping.city} ${new_shipping.postcode}`,
            countries[new_shipping.country],
        ].filter((line) => line);
        const prompt =
            `Please change the shipping address on my Parcelpost account to ${
                lines.join(", ")
            }. Leave my billing address as it is. If you need to sign in, my email is ${account.email} and my password is ${account.password}.`;
        return { prompt, site: openSite(scenario) };
    },
};
