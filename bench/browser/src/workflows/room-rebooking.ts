import {
    errorResponse,
    htmlResponse,
    notFound,
    pause,
    readObject,
    renderPage,
    routeOf,
    seededRandom,
    settledSite,
    twoDigits,
    weekdayDate,
    type Workflow,
    type WorkflowSite,
} from "./workflow.ts";

export type Room = { id: string; name: string; seats: number; floor: number };

export type Booking = {
    room_id: string;
    date: string;
    hour: number;
    title: string;
    owner: string;
};

export type RoomRebookingScenario = {
    rooms: Room[];
    bookings: Booking[];
    days: string[];
    title: string;
    min_seats: number;
    requested: { room_id: string; date: string; hour: number };
    rival: Booking;
    expected: { room_id: string; date: string; hour: number };
};

const room_names = [
    "Juniper",
    "Alder",
    "Birch",
    "Cedar",
    "Hazel",
    "Linden",
    "Maple",
    "Rowan",
    "Willow",
];
const seat_counts = [4, 5, 6, 8, 10, 12];
const titles = [
    "Quarterly planning",
    "Vendor review",
    "Hiring debrief",
    "Roadmap sync",
    "Incident retro",
];
const colleagues = ["Ines", "Marek", "Yuki", "Bola", "Sven", "Clara"];
const days = ["2026-05-12", "2026-05-13", "2026-05-14"];
const first_hour = 9;
const last_hour = 16;
const you = "you";
const schedule_latency_ms = 250;
const booking_latency_ms = 200;

export function roomRebookingScenario(seed: number): RoomRebookingScenario {
    const random = seededRandom(seed);
    const seats = random.shuffle(seat_counts);
    const rooms = random.sample(room_names, seat_counts.length).map((
        name,
        index,
    ) => ({
        id: name.toLowerCase(),
        name,
        seats: seats[index]!,
        floor: random.int(1, 4),
    }));
    const min_seats = random.pick([6, 8]);
    const large = rooms.filter((room) => room.seats >= min_seats);
    const requested_room = random.pick(large);
    const fallback_room = random.pick(large);
    const date = random.pick(days);
    const hour = random.int(first_hour, 13);
    const fallback_hour = hour + random.int(1, 3);

    const busy = new Set<string>();
    const key = (room_id: string, day: string, slot: number) =>
        `${room_id}|${day}|${slot}`;
    for (const room of large) {
        for (let slot = hour + 1; slot <= fallback_hour; slot++) {
            if (slot === fallback_hour && room === fallback_room) continue;
            busy.add(key(room.id, date, slot));
        }
    }
    const fixed = new Set([
        key(requested_room.id, date, hour),
        key(fallback_room.id, date, fallback_hour),
    ]);
    for (const day of days) {
        for (const room of rooms) {
            for (let slot = first_hour; slot <= last_hour; slot++) {
                const inside_window = day === date && room.seats >= min_seats &&
                    slot > hour && slot <= fallback_hour;
                if (inside_window || fixed.has(key(room.id, day, slot))) {
                    continue;
                }
                if (random.next() < 0.35) busy.add(key(room.id, day, slot));
            }
        }
    }
    const bookings = [...busy].sort().map((entry) => {
        const [room_id, day, slot] = entry.split("|") as [
            string,
            string,
            string,
        ];
        return {
            room_id,
            date: day,
            hour: Number(slot),
            title: "Private meeting",
            owner: random.pick(colleagues),
        };
    });
    return {
        rooms,
        bookings,
        days,
        title: random.pick(titles),
        min_seats,
        requested: { room_id: requested_room.id, date, hour },
        rival: {
            room_id: requested_room.id,
            date,
            hour,
            title: "Private meeting",
            owner: random.pick(colleagues),
        },
        expected: { room_id: fallback_room.id, date, hour: fallback_hour },
    };
}

export function slotLabel(hour: number): string {
    return `${twoDigits(hour)}:00–${twoDigits(hour + 1)}:00`;
}

const page = renderPage({
    title: "Room bookings",
    brand: "Fernhall · Rooms",
    style: `
  #days { display: flex; gap: 8px; margin-bottom: 12px; }
  #days button[aria-pressed=true] { background: #1f3a5f; color: #fff; border-color: #1f3a5f; }
  td.busy { color: #5b6470; background: #f0f1f3; }
  td.free button { width: 100%; }
  #grid th, #grid td { text-align: center; padding: 6px; }
  #grid th[scope=row] { text-align: left; }
`,
    body: `
<main>
  <h1>Room bookings</h1>
  <p id="alert" class="error" role="alert"></p>
  <p id="notice" class="notice" role="status" aria-live="polite"></p>
  <nav id="days" aria-label="Day"></nav>
  <p id="loading" role="status">Loading schedule…</p>
  <table id="grid" aria-label="Room schedule">
    <thead><tr id="hours"></tr></thead>
    <tbody id="rows"></tbody>
  </table>
  <h2>Your bookings</h2>
  <ul id="mine"></ul>
</main>
<dialog id="book" aria-labelledby="book-title">
  <form id="book-form">
    <h2 id="book-title">Book room</h2>
    <p id="book-summary"></p>
    <label for="meeting-title">Meeting title</label>
    <input type="text" id="meeting-title" name="title" autocomplete="off">
    <p id="book-error" class="error" role="alert"></p>
    <div class="actions">
      <button type="button" id="book-cancel">Cancel</button>
      <button type="submit" class="primary" id="book-submit">Confirm booking</button>
    </div>
  </form>
</dialog>
`,
    script: `
let rooms = [];
let days = [];
let current_day = null;
let current_slot = null;
function hourLabel(hour) {
    return String(hour).padStart(2, "0") + ":00";
}
function slotLabel(hour) {
    return hourLabel(hour) + "–" + hourLabel(hour + 1);
}
function renderDays() {
    byId("days").replaceChildren(...days.map((day) =>
        h("button", { type: "button", "aria-pressed": day.date === current_day ? "true" : "false", onclick: () => loadSchedule(day.date) }, day.label)));
}
async function loadSchedule(date) {
    current_day = date;
    renderDays();
    byId("loading").hidden = false;
    byId("grid").setAttribute("aria-busy", "true");
    const result = await api("GET", "/api/schedule?date=" + date);
    if (current_day !== date) return;
    byId("loading").hidden = true;
    byId("grid").setAttribute("aria-busy", "false");
    if (!result.ok) {
        byId("alert").textContent = "The schedule could not be loaded.";
        return;
    }
    const schedule = result.data;
    byId("hours").replaceChildren(h("th", { scope: "col" }, "Room"), ...schedule.hours.map((hour) => h("th", { scope: "col" }, hourLabel(hour))));
    byId("rows").replaceChildren(...rooms.map((room) => {
        const taken = new Set(schedule.busy.filter((slot) => slot.room_id === room.id).map((slot) => slot.hour));
        return h("tr", {},
            h("th", { scope: "row" }, room.name + " · " + room.seats + " seats · floor " + room.floor),
            ...schedule.hours.map((hour) => taken.has(hour)
                ? h("td", { class: "busy" }, "Booked")
                : h("td", { class: "free" }, h("button", { type: "button", "aria-label": "Book " + room.name + " " + schedule.label + " " + slotLabel(hour), onclick: () => openBooking(room, schedule, hour) }, "Free"))));
    }));
    byId("mine").replaceChildren(...schedule.mine.map((booking) => h("li", {}, booking.title + ": " + booking.room_name + ", " + booking.day_label + ", " + slotLabel(booking.hour))));
    if (schedule.mine.length === 0) byId("mine").append(h("li", {}, "No bookings yet."));
}
function openBooking(room, schedule, hour) {
    current_slot = { room, date: schedule.date, hour };
    byId("book-form").reset();
    byId("book-title").textContent = "Book " + room.name;
    byId("book-summary").textContent = room.name + " (" + room.seats + " seats), " + schedule.label + ", " + slotLabel(hour) + ".";
    byId("book-error").textContent = "";
    byId("book").showModal();
}
byId("book-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const title = byId("meeting-title").value.trim();
    if (!title) {
        byId("book-error").textContent = "Enter a meeting title.";
        return;
    }
    byId("book-submit").disabled = true;
    const result = await api("POST", "/api/bookings", { room_id: current_slot.room.id, date: current_slot.date, hour: current_slot.hour, title });
    byId("book-submit").disabled = false;
    if (result.status === 409) {
        byId("book").close();
        byId("notice").textContent = "";
        byId("alert").textContent = (result.data && result.data.error) || "That slot is no longer available.";
        await loadSchedule(current_slot.date);
        return;
    }
    if (!result.ok) {
        byId("book-error").textContent = (result.data && result.data.error) || "The booking could not be made.";
        return;
    }
    byId("book").close();
    byId("alert").textContent = "";
    byId("notice").textContent = "Booked " + current_slot.room.name + ", " + slotLabel(current_slot.hour) + ".";
    await loadSchedule(current_slot.date);
});
byId("book-cancel").addEventListener("click", () => byId("book").close());
(async () => {
    const result = await api("GET", "/api/rooms");
    rooms = result.data.rooms;
    days = result.data.days;
    await loadSchedule(days[0].date);
})();
`,
});

function openSite(scenario: RoomRebookingScenario): WorkflowSite {
    const bookings = scenario.bookings.map((booking) => ({ ...booking }));
    let rival_pending = true;
    const roomOf = (id: unknown) =>
        scenario.rooms.find((room) => room.id === id);

    return settledSite({
        async handle(request) {
            const { method, path, query } = routeOf(request);
            if (method === "GET" && path === "/") return htmlResponse(page);
            if (method === "GET" && path === "/api/rooms") {
                await pause(booking_latency_ms);
                return Response.json({
                    rooms: scenario.rooms,
                    days: scenario.days.map((date) => ({
                        date,
                        label: weekdayDate(date),
                    })),
                });
            }
            if (method === "GET" && path === "/api/schedule") {
                await pause(schedule_latency_ms);
                const date = query.get("date") ?? "";
                if (!scenario.days.includes(date)) {
                    return errorResponse(400, "Unknown day.");
                }
                return Response.json({
                    date,
                    label: weekdayDate(date),
                    hours: Array.from(
                        { length: last_hour - first_hour + 1 },
                        (_, index) => first_hour + index,
                    ),
                    busy: bookings.filter((booking) => booking.date === date)
                        .map(({ room_id, hour }) => ({ room_id, hour })),
                    mine: bookings.filter((booking) => booking.owner === you)
                        .map((booking) => ({
                            room_name: roomOf(booking.room_id)!.name,
                            day_label: weekdayDate(booking.date),
                            hour: booking.hour,
                            title: booking.title,
                        })),
                });
            }
            if (method === "POST" && path === "/api/bookings") {
                await pause(booking_latency_ms);
                const body = await readObject(request);
                if (!body) return errorResponse(400, "Malformed request.");
                const room = roomOf(body.room_id);
                const { date, hour } = body;
                const title = typeof body.title === "string"
                    ? body.title.trim()
                    : "";
                if (
                    !room || typeof date !== "string" ||
                    !scenario.days.includes(date) ||
                    typeof hour !== "number" || !Number.isInteger(hour) ||
                    hour < first_hour || hour > last_hour
                ) {
                    return errorResponse(400, "Choose a listed room and slot.");
                }
                if (!title || title.length > 80) {
                    return errorResponse(
                        400,
                        "Enter a meeting title (80 characters at most).",
                        { title: "invalid" },
                    );
                }
                const { rival } = scenario;
                if (
                    rival_pending && room.id === rival.room_id &&
                    date === rival.date && hour === rival.hour
                ) {
                    rival_pending = false;
                    bookings.push({ ...rival });
                }
                if (
                    bookings.some((booking) =>
                        booking.room_id === room.id && booking.date === date &&
                        booking.hour === hour
                    )
                ) {
                    return errorResponse(
                        409,
                        `Sorry, ${room.name} was booked by someone else for ${
                            slotLabel(hour)
                        } on ${weekdayDate(date)}.`,
                    );
                }
                bookings.push({
                    room_id: room.id,
                    date,
                    hour,
                    title,
                    owner: you,
                });
                return Response.json({ booked: true }, { status: 201 });
            }
            return notFound();
        },
        outcome() {
            const mine = bookings.filter((booking) => booking.owner === you);
            if (mine.length === 0) return "no booking was made";
            if (mine.length > 1) {
                return `${mine.length} bookings were made, expected one`;
            }
            const booking = mine[0]!;
            const { expected } = scenario;
            if (
                booking.room_id !== expected.room_id ||
                booking.date !== expected.date ||
                booking.hour !== expected.hour
            ) {
                return `booked ${booking.room_id} on ${booking.date} at ${booking.hour}:00, expected ${expected.room_id} on ${expected.date} at ${expected.hour}:00`;
            }
            if (booking.title.toLowerCase() !== scenario.title.toLowerCase()) {
                return `booking title was "${booking.title}", expected "${scenario.title}"`;
            }
            return null;
        },
    });
}

export const room_rebooking: Workflow = {
    id: "room-rebooking",
    covers: ["recovery", "dialogs", "async-loading"],
    seeds: [41, 42, 43],
    open(seed) {
        const scenario = roomRebookingScenario(seed);
        const { requested } = scenario;
        const room = scenario.rooms.find((entry) =>
            entry.id === requested.room_id
        )!;
        const prompt = `Please book ${room.name} on ${
            weekdayDate(requested.date)
        } from ${
            slotLabel(requested.hour).replace("–", " to ")
        } for "${scenario.title}". If that time is already taken, book the earliest free one-hour slot later that same day in any room with at least ${scenario.min_seats} seats instead.`;
        return { prompt, site: openSite(scenario) };
    },
};
