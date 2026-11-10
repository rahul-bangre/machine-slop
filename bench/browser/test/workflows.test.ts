import assert from "node:assert/strict";

import { addressChangeScenario } from "../src/workflows/address-change.ts";
import { expenseClaimScenario } from "../src/workflows/expense-claim.ts";
import {
    dispute_reasons,
    invoiceDisputeScenario,
} from "../src/workflows/invoice-dispute.ts";
import { memberOffboardingScenario } from "../src/workflows/member-offboarding.ts";
import { type Capability, workflows } from "../src/workflows/mod.ts";
import type { WorkflowSite } from "../src/workflows/workflow.ts";
import { roomRebookingScenario } from "../src/workflows/room-rebooking.ts";

type Reply<T> = {
    status: number;
    data: T;
    location: string | null;
    cookie: string | null;
};

async function call<T = Record<string, unknown>>(
    site: WorkflowSite,
    method: string,
    path: string,
    body?: unknown,
    cookie?: string,
): Promise<Reply<T>> {
    const headers: Record<string, string> = {};
    if (body !== undefined) headers["content-type"] = "application/json";
    if (cookie) headers.cookie = cookie;
    const response = await site.handle(
        new Request(`http://site${path}`, {
            method,
            headers,
            body: body === undefined ? null : JSON.stringify(body),
        }),
    );
    const text = await response.text();
    let data: unknown = text;
    try {
        data = JSON.parse(text);
    } catch {
        data = text;
    }
    return {
        status: response.status,
        data: data as T,
        location: response.headers.get("location"),
        cookie: response.headers.get("set-cookie")?.split(";")[0] ?? null,
    };
}

function workflow(id: string) {
    const found = workflows.find((entry) => entry.id === id);
    assert.ok(found, id);
    return found;
}

function forEachSeed(
    id: string,
    check: (seed: number) => Promise<void>,
): Promise<void[]> {
    return Promise.all(workflow(id).seeds.map(check));
}

async function fails(site: WorkflowSite, why: string): Promise<void> {
    assert.notEqual(await site.outcome(), null, why);
}

type InvoiceRow = {
    id: string;
    customer: string;
    issued_on: string;
    amount_cents: number;
    status: string;
};

async function readInvoices(site: WorkflowSite): Promise<InvoiceRow[]> {
    const rows: InvoiceRow[] = [];
    let offset: number | null = 0;
    while (offset !== null) {
        const page: Reply<
            { invoices: InvoiceRow[]; next_offset: number | null }
        > = await call(site, "GET", `/api/invoices?offset=${offset}`);
        assert.equal(page.status, 200);
        rows.push(...page.data.invoices);
        offset = page.data.next_offset;
    }
    return rows;
}

function largestUnpaid(
    rows: InvoiceRow[],
    matches: (row: InvoiceRow) => boolean,
): InvoiceRow[] {
    return rows.filter((row) => row.status === "unpaid" && matches(row))
        .sort((left, right) => right.amount_cents - left.amount_cents);
}

async function solveInvoiceDispute(site: WorkflowSite, seed: number) {
    const scenario = invoiceDisputeScenario(seed);
    const month = `2026-${String(scenario.month).padStart(2, "0")}`;
    const [target] = largestUnpaid(
        await readInvoices(site),
        (row) =>
            row.customer === scenario.customer &&
            row.issued_on.startsWith(month),
    );
    const reply = await call(
        site,
        "POST",
        `/api/invoices/${target!.id}/dispute`,
        {
            reason: scenario.reason,
        },
    );
    assert.equal(reply.status, 200);
}

type MemberRow = { id: string; name: string; email: string };

async function readMembers(site: WorkflowSite) {
    const members =
        (await call<{ members: MemberRow[] }>(site, "GET", "/api/members"))
            .data.members;
    return (name: string) => members.find((member) => member.name === name)!;
}

function memberNames(seed: number) {
    const scenario = memberOffboardingScenario(seed);
    const nameOf = (id: string) =>
        scenario.members.find((member) => member.id === id)!.name;
    return {
        target: nameOf(scenario.target_id),
        recipient: nameOf(scenario.recipient_id),
        lookalike: nameOf(scenario.lookalike_id),
        recipient_lookalike: nameOf(scenario.recipient_lookalike_id),
    };
}

function removeMember(
    site: WorkflowSite,
    member: MemberRow,
    transfer_to: string | null,
) {
    return call(site, "POST", `/api/members/${member.id}/remove`, {
        transfer_to,
        confirm_email: member.email,
    });
}

async function solveMemberOffboarding(site: WorkflowSite, seed: number) {
    const names = memberNames(seed);
    const find = await readMembers(site);
    const target = find(names.target);
    const projects = await call<{ projects: unknown[] }>(
        site,
        "GET",
        `/api/members/${target.id}/projects`,
    );
    assert.ok(projects.data.projects.length > 0);
    const reply = await removeMember(site, target, find(names.recipient).id);
    assert.equal(reply.status, 200);
}

async function costCentreId(site: WorkflowSite, seed: number, region?: string) {
    const scenario = expenseClaimScenario(seed);
    const search = await call<{ cost_centres: { id: string; name: string }[] }>(
        site,
        "GET",
        `/api/cost-centres?q=${
            encodeURIComponent(scenario.department.toLowerCase())
        }`,
    );
    return search.data.cost_centres.find((entry) =>
        entry.name === `${scenario.department} – ${region ?? scenario.region}`
    )!.id;
}

async function referenceClaim(site: WorkflowSite, seed: number) {
    const { expected, purpose } = expenseClaimScenario(seed);
    return {
        category: expected.category,
        merchant: expected.merchant,
        expense_date: expected.expense_date,
        purpose: `Expenses for ${purpose}`,
        amount: (expected.amount_cents / 100).toFixed(2),
        currency: expected.currency,
        trip_start: expected.trip_start,
        trip_end: expected.trip_end,
        attendees: expected.attendees,
        cost_centre_id: await costCentreId(site, seed),
    };
}

async function solveExpenseClaim(site: WorkflowSite, seed: number) {
    const reply = await call(
        site,
        "POST",
        "/api/claims",
        await referenceClaim(site, seed),
    );
    assert.equal(reply.status, 201);
}

type Schedule = { busy: { room_id: string; hour: number }[]; hours: number[] };
type RoomRow = { id: string; name: string; seats: number };

async function readSchedule(site: WorkflowSite, date: string) {
    const schedule = await call<Schedule>(
        site,
        "GET",
        `/api/schedule?date=${date}`,
    );
    assert.equal(schedule.status, 200);
    const busy = new Set(
        schedule.data.busy.map((slot) => `${slot.room_id}|${slot.hour}`),
    );
    return {
        hours: schedule.data.hours,
        isFree: (room_id: string, hour: number) =>
            !busy.has(`${room_id}|${hour}`),
    };
}

function book(
    site: WorkflowSite,
    room_id: string,
    date: string,
    hour: number,
    title: string,
) {
    return call(site, "POST", "/api/bookings", { room_id, date, hour, title });
}

async function attemptRequestedRoom(site: WorkflowSite, seed: number) {
    const scenario = roomRebookingScenario(seed);
    const { requested } = scenario;
    const rooms =
        (await call<{ rooms: RoomRow[] }>(site, "GET", "/api/rooms")).data
            .rooms;
    const before = await readSchedule(site, requested.date);
    assert.ok(before.isFree(requested.room_id, requested.hour));
    const attempt = await book(
        site,
        requested.room_id,
        requested.date,
        requested.hour,
        scenario.title,
    );
    assert.equal(attempt.status, 409);
    const after = await readSchedule(site, requested.date);
    assert.ok(!after.isFree(requested.room_id, requested.hour));
    return { scenario, rooms, after };
}

async function solveRoomRebooking(site: WorkflowSite, seed: number) {
    const { scenario, rooms, after } = await attemptRequestedRoom(site, seed);
    const { requested } = scenario;
    const large = rooms.filter((room) => room.seats >= scenario.min_seats);
    const hour = after.hours.find((slot) =>
        slot > requested.hour &&
        large.some((room) => after.isFree(room.id, slot))
    )!;
    const free = large.filter((room) => after.isFree(room.id, hour));
    assert.equal(free.length, 1);
    const reply = await book(
        site,
        free[0]!.id,
        requested.date,
        hour,
        scenario.title,
    );
    assert.equal(reply.status, 201);
}

async function signIn(site: WorkflowSite, seed: number) {
    const { account } = addressChangeScenario(seed);
    const reply = await call(site, "POST", "/api/sign-in", {
        email: account.email.toUpperCase(),
        password: account.password,
    });
    assert.equal(reply.status, 200);
    return reply.cookie!;
}

async function signInPastExpiry(site: WorkflowSite, seed: number) {
    const { new_shipping } = addressChangeScenario(seed);
    const start = await call(site, "GET", "/");
    assert.equal(start.status, 303);
    assert.equal(start.location, "/sign-in");
    const first = await signIn(site, seed);
    assert.equal(
        (await call(site, "GET", "/api/profile", undefined, first)).status,
        200,
    );
    const lost = await call(
        site,
        "PUT",
        "/api/addresses/shipping",
        new_shipping,
        first,
    );
    assert.equal(lost.status, 401);
    const back = await call(site, "GET", "/", undefined, first);
    assert.equal(back.location, "/sign-in?expired=1");
    return await signIn(site, seed);
}

async function solveAddressChange(site: WorkflowSite, seed: number) {
    const { new_shipping } = addressChangeScenario(seed);
    const cookie = await signInPastExpiry(site, seed);
    const saved = await call(site, "PUT", "/api/addresses/shipping", {
        ...new_shipping,
        postcode: new_shipping.postcode.toLowerCase().replace(" ", ""),
    }, cookie);
    assert.equal(saved.status, 200);
}

const references: Record<
    string,
    (site: WorkflowSite, seed: number) => Promise<void>
> = {
    "invoice-dispute": solveInvoiceDispute,
    "member-offboarding": solveMemberOffboarding,
    "expense-claim": solveExpenseClaim,
    "room-rebooking": solveRoomRebooking,
    "address-change": solveAddressChange,
};

const banned_prompt_words = [
    "click",
    "tap",
    "button",
    "select",
    "scroll",
    "load more",
    "dropdown",
    "dialog",
    "modal",
    "menu",
    "http",
    "/api",
    "url",
    "seed",
    "benchmark",
    "conflict",
    "expire",
];

Deno.test("the pack covers every capability with several seeds per workflow", () => {
    const covered = new Set<Capability>(
        workflows.flatMap((entry) => entry.covers),
    );
    assert.deepEqual([...covered].sort(), [
        "async-loading",
        "dialogs",
        "multi-step-form",
        "recovery",
    ]);
    assert.equal(
        new Set(workflows.map((entry) => entry.id)).size,
        workflows.length,
    );
    for (const entry of workflows) {
        assert.ok(entry.seeds.length >= 3, entry.id);
        assert.ok(references[entry.id], `reference solution for ${entry.id}`);
    }
});

Deno.test("prompts state a goal without interface hints and differ per seed", () => {
    for (const entry of workflows) {
        const prompts = entry.seeds.map((seed) => entry.open(seed).prompt);
        assert.equal(new Set(prompts).size, prompts.length, entry.id);
        assert.deepEqual(
            entry.seeds.map((seed) => entry.open(seed).prompt),
            prompts,
        );
        for (const prompt of prompts) {
            for (const word of banned_prompt_words) {
                assert.ok(
                    !new RegExp(`\\b${word}`, "i").test(prompt),
                    `${entry.id} prompt mentions "${word}": ${prompt}`,
                );
            }
        }
    }
});

Deno.test("each workflow's start page is html and its outcome fails until a reference solution runs", async () => {
    await Promise.all(
        workflows.flatMap((entry) =>
            entry.seeds.map(async (seed) => {
                const { site } = entry.open(seed);
                const untouched = entry.open(seed).site;
                await fails(site, `${entry.id}/${seed} fresh`);
                if (entry.id !== "address-change") {
                    const start = await site.handle(
                        new Request("http://site/"),
                    );
                    assert.match(
                        start.headers.get("content-type") ?? "",
                        /text\/html/,
                    );
                    assert.match(await start.text(), /<dialog|<form/);
                }
                await references[entry.id]!(site, seed);
                assert.equal(await site.outcome(), null, `${entry.id}/${seed}`);
                await fails(
                    untouched,
                    `${entry.id}/${seed} separate open shares state`,
                );
            })
        ),
    );
});

Deno.test("invoice-dispute rejects the wrong invoice, extra disputes, wrong reasons and paid invoices", async () => {
    await forEachSeed("invoice-dispute", async (seed) => {
        const scenario = invoiceDisputeScenario(seed);
        const prompt = workflow("invoice-dispute").open(seed).prompt;
        assert.ok(prompt.includes(scenario.customer));
        assert.ok(prompt.includes(dispute_reasons[scenario.reason]!));
        const month = `2026-${String(scenario.month).padStart(2, "0")}`;
        const ofCustomer = (row: InvoiceRow) =>
            row.customer === scenario.customer;
        const inMonth = (row: InvoiceRow) => row.issued_on.startsWith(month);
        const other_reason = Object.keys(dispute_reasons).find((reason) =>
            reason !== scenario.reason
        )!;
        const dispute = (
            site: WorkflowSite,
            id: string,
            reason = scenario.reason,
        ) => call(site, "POST", `/api/invoices/${id}/dispute`, { reason });
        await Promise.all([
            (async () => {
                const any_month = workflow("invoice-dispute").open(seed).site;
                const [largest_any_month] = largestUnpaid(
                    await readInvoices(any_month),
                    ofCustomer,
                );
                assert.notEqual(largest_any_month!.id, scenario.target_id);
                await dispute(any_month, largest_any_month!.id);
                await fails(any_month, "largest unpaid in any month");
            })(),
            (async () => {
                const twice = workflow("invoice-dispute").open(seed).site;
                const [target, runner_up] = largestUnpaid(
                    await readInvoices(twice),
                    (row) => ofCustomer(row) && inMonth(row),
                );
                assert.equal(target!.id, scenario.target_id);
                await dispute(twice, target!.id);
                await dispute(twice, runner_up!.id);
                await fails(twice, "two disputes");
            })(),
            (async () => {
                const wrong_reason =
                    workflow("invoice-dispute").open(seed).site;
                await dispute(wrong_reason, scenario.target_id, other_reason);
                await fails(wrong_reason, "wrong reason");
            })(),
            (async () => {
                const invalid = workflow("invoice-dispute").open(seed).site;
                const paid = scenario.invoices.find((row) =>
                    row.status === "paid"
                )!;
                assert.equal((await dispute(invalid, paid.id)).status, 409);
                assert.equal(
                    (await dispute(invalid, scenario.target_id, "because"))
                        .status,
                    400,
                );
                assert.equal(
                    (await dispute(invalid, scenario.target_id)).status,
                    200,
                );
                assert.equal(
                    (await dispute(invalid, scenario.target_id)).status,
                    409,
                );
                assert.equal(await invalid.outcome(), null);
            })(),
        ]);
    });
});

Deno.test("member-offboarding rejects lookalikes, wrong recipients, extra removals and unconfirmed removals", async () => {
    await forEachSeed("member-offboarding", async (seed) => {
        const names = memberNames(seed);
        const prompt = workflow("member-offboarding").open(seed).prompt;
        assert.ok(
            prompt.includes(names.target) && prompt.includes(names.recipient),
        );
        await Promise.all([
            (async () => {
                const lookalike =
                    workflow("member-offboarding").open(seed).site;
                const find = await readMembers(lookalike);
                await removeMember(
                    lookalike,
                    find(names.lookalike),
                    find(names.recipient).id,
                );
                await fails(lookalike, "removed the lookalike");
            })(),
            (async () => {
                const wrong_recipient =
                    workflow("member-offboarding").open(seed).site;
                const find = await readMembers(wrong_recipient);
                await removeMember(
                    wrong_recipient,
                    find(names.target),
                    find(names.recipient_lookalike).id,
                );
                await fails(
                    wrong_recipient,
                    "projects went to the recipient's lookalike",
                );
            })(),
            (async () => {
                const extra = workflow("member-offboarding").open(seed).site;
                await solveMemberOffboarding(extra, seed);
                const find = await readMembers(extra);
                await removeMember(
                    extra,
                    find(names.lookalike),
                    find(names.recipient).id,
                );
                await fails(extra, "removed an extra member");
            })(),
            (async () => {
                const invalid = workflow("member-offboarding").open(seed).site;
                const find = await readMembers(invalid);
                const target = find(names.target);
                const recipient = find(names.recipient).id;
                const attempt = (body: unknown) =>
                    call(
                        invalid,
                        "POST",
                        `/api/members/${target.id}/remove`,
                        body,
                    );
                assert.equal(
                    (await attempt({
                        transfer_to: recipient,
                        confirm_email: names.target,
                    })).status,
                    400,
                );
                assert.equal(
                    (await attempt({
                        transfer_to: target.id,
                        confirm_email: target.email,
                    })).status,
                    400,
                );
                assert.equal(
                    (await attempt({
                        transfer_to: null,
                        confirm_email: target.email,
                    }))
                        .status,
                    400,
                );
                assert.equal(
                    (await call(invalid, "POST", "/api/members/m-100/remove", {
                        transfer_to: recipient,
                        confirm_email: "sam.reyes@harbourline.test",
                    })).status,
                    400,
                );
                await fails(invalid, "invalid removals");
                assert.equal(await invalid.outcome(), "no member was removed");
            })(),
        ]);
    });
});

Deno.test("expense-claim rejects wrong cost centres, duplicates, wrong amounts and invalid claims", async () => {
    await forEachSeed("expense-claim", async (seed) => {
        const scenario = expenseClaimScenario(seed);
        const prompt = workflow("expense-claim").open(seed).prompt;
        for (
            const fact of [
                scenario.expected.merchant,
                scenario.department,
                scenario.region,
                scenario.purpose_keyword,
            ]
        ) {
            assert.ok(prompt.includes(fact), fact);
        }
        const submit = (site: WorkflowSite, body: unknown) =>
            call<{ fields: Record<string, string> }>(
                site,
                "POST",
                "/api/claims",
                body,
            );
        await Promise.all([
            (async () => {
                const wrong_region = workflow("expense-claim").open(seed).site;
                const other_region = ["Nordics", "Baltics", "Iberia", "Benelux"]
                    .find((
                        region,
                    ) => region !== scenario.region)!;
                await submit(wrong_region, {
                    ...await referenceClaim(wrong_region, seed),
                    cost_centre_id: await costCentreId(
                        wrong_region,
                        seed,
                        other_region,
                    ),
                });
                await fails(wrong_region, "same department, other region");
            })(),
            (async () => {
                const twice = workflow("expense-claim").open(seed).site;
                await solveExpenseClaim(twice, seed);
                await solveExpenseClaim(twice, seed);
                await fails(twice, "duplicate claim");
            })(),
            (async () => {
                const wrong_amount = workflow("expense-claim").open(seed).site;
                await submit(wrong_amount, {
                    ...await referenceClaim(wrong_amount, seed),
                    amount: ((scenario.expected.amount_cents + 100) / 100)
                        .toFixed(2),
                });
                await fails(wrong_amount, "wrong amount");
            })(),
            (async () => {
                const invalid = workflow("expense-claim").open(seed).site;
                const claim = await referenceClaim(invalid, seed);
                const travel = claim.category === "travel";
                const missing_section = travel
                    ? { ...claim, trip_start: null, trip_end: null }
                    : { ...claim, attendees: null };
                const crossed_section = travel ? { ...claim, attendees: 3 } : {
                    ...claim,
                    trip_start: claim.expense_date,
                    trip_end: claim.expense_date,
                };
                for (
                    const body of [
                        missing_section,
                        crossed_section,
                        { ...claim, cost_centre_id: "cc-0000" },
                        { ...claim, amount: "12.345" },
                        { ...claim, expense_date: "2026-06-30" },
                        { ...claim, expense_date: "12/03/2026" },
                    ]
                ) {
                    assert.equal(
                        (await submit(invalid, body)).status,
                        422,
                        JSON.stringify(body),
                    );
                }
                assert.equal(await invalid.outcome(), "no claim was submitted");
            })(),
        ]);
    });
});

Deno.test("room-rebooking rejects small rooms, double bookings and invalid slots", async () => {
    await forEachSeed("room-rebooking", async (seed) => {
        const scenario = roomRebookingScenario(seed);
        const prompt = workflow("room-rebooking").open(seed).prompt;
        assert.ok(
            prompt.includes(scenario.title) &&
                prompt.includes(`${scenario.min_seats} seats`),
        );
        await Promise.all([
            (async () => {
                const small = workflow("room-rebooking").open(seed).site;
                const attempted = await attemptRequestedRoom(small, seed);
                const small_free = attempted.after.hours.flatMap((hour) =>
                    attempted.rooms.filter((room) =>
                        room.seats < scenario.min_seats &&
                        hour > scenario.requested.hour &&
                        attempted.after.isFree(room.id, hour)
                    ).map((room) => ({ room, hour }))
                )[0]!;
                assert.equal(
                    (await book(
                        small,
                        small_free.room.id,
                        scenario.requested.date,
                        small_free.hour,
                        scenario.title,
                    )).status,
                    201,
                );
                await fails(small, "room below the seat minimum");
            })(),
            (async () => {
                const twice = workflow("room-rebooking").open(seed).site;
                await solveRoomRebooking(twice, seed);
                const other_day = scenario.days.find((day) =>
                    day !== scenario.requested.date
                )!;
                const schedule = await readSchedule(twice, other_day);
                const free_hour = schedule.hours.find((hour) =>
                    schedule.isFree(scenario.expected.room_id, hour)
                )!;
                assert.equal(
                    (await book(
                        twice,
                        scenario.expected.room_id,
                        other_day,
                        free_hour,
                        scenario.title,
                    )).status,
                    201,
                );
                await fails(twice, "two bookings");
            })(),
            (async () => {
                const invalid = workflow("room-rebooking").open(seed).site;
                const { expected } = scenario;
                assert.equal(
                    (await book(
                        invalid,
                        expected.room_id,
                        expected.date,
                        17,
                        scenario.title,
                    )).status,
                    400,
                );
                assert.equal(
                    (await book(
                        invalid,
                        expected.room_id,
                        "2026-05-15",
                        expected.hour,
                        scenario.title,
                    )).status,
                    400,
                );
                assert.equal(
                    (await book(
                        invalid,
                        expected.room_id,
                        expected.date,
                        expected.hour,
                        "  ",
                    )).status,
                    400,
                );
                const taken = scenario.bookings.find((booking) =>
                    booking.date === expected.date
                )!;
                assert.equal(
                    (await book(
                        invalid,
                        taken.room_id,
                        taken.date,
                        taken.hour,
                        scenario.title,
                    )).status,
                    409,
                );
                assert.equal(await invalid.outcome(), "no booking was made");
            })(),
        ]);
    });
});

Deno.test("address-change rejects billing edits, wrong addresses and unauthenticated or invalid saves", async () => {
    await forEachSeed("address-change", async (seed) => {
        const { account, new_shipping } = addressChangeScenario(seed);
        const prompt = workflow("address-change").open(seed).prompt;
        for (
            const fact of [
                account.email,
                account.password,
                new_shipping.line1,
                new_shipping.postcode,
            ]
        ) {
            assert.ok(prompt.includes(fact), fact);
        }
        const save = (
            site: WorkflowSite,
            kind: string,
            address: unknown,
            cookie?: string,
        ) => call(site, "PUT", `/api/addresses/${kind}`, address, cookie);
        await Promise.all([
            (async () => {
                const split = workflow("address-change").open(seed).site;
                const merged = {
                    ...new_shipping,
                    line1: [new_shipping.line1, new_shipping.line2].filter((
                        line,
                    ) => line).join(", "),
                    line2: "",
                };
                assert.equal(
                    (await save(
                        split,
                        "shipping",
                        merged,
                        await signInPastExpiry(split, seed),
                    )).status,
                    200,
                );
                assert.equal(
                    await split.outcome(),
                    null,
                    "address lines merged into one",
                );
            })(),
            (async () => {
                const billing_too = workflow("address-change").open(seed).site;
                await solveAddressChange(billing_too, seed);
                const cookie = await signIn(billing_too, seed);
                assert.equal(
                    (await save(billing_too, "billing", new_shipping, cookie))
                        .status,
                    200,
                );
                await fails(billing_too, "billing changed as well");
            })(),
            (async () => {
                const wrong_city = workflow("address-change").open(seed).site;
                assert.equal(
                    (await save(wrong_city, "shipping", {
                        ...new_shipping,
                        city: account.shipping.city,
                    }, await signInPastExpiry(wrong_city, seed))).status,
                    200,
                );
                await fails(wrong_city, "wrong city");
            })(),
            (async () => {
                const invalid = workflow("address-change").open(seed).site;
                assert.equal(
                    (await save(invalid, "shipping", new_shipping)).status,
                    401,
                );
                assert.equal(
                    (await call(invalid, "POST", "/api/sign-in", {
                        email: account.email,
                        password: "wrong",
                    })).status,
                    401,
                );
                const fresh = await signInPastExpiry(invalid, seed);
                const other_country = new_shipping.country === "DE"
                    ? "NL"
                    : "DE";
                assert.equal(
                    (await save(invalid, "shipping", {
                        ...new_shipping,
                        country: other_country,
                    }, fresh)).status,
                    422,
                );
                assert.equal(
                    (await save(
                        invalid,
                        "shipping",
                        { ...new_shipping, line1: " " },
                        fresh,
                    )).status,
                    422,
                );
                await fails(invalid, "invalid or unauthenticated saves");
            })(),
        ]);
    });
});

Deno.test("the outcome includes writes the site accepted before it was asked", async () => {
    await forEachSeed("expense-claim", async (seed) => {
        const site = workflow("expense-claim").open(seed).site;
        const claim = await referenceClaim(site, seed);
        const submitted = call(site, "POST", "/api/claims", claim);
        assert.equal(await site.outcome(), null);
        assert.equal((await submitted).status, 201);
    });
});
