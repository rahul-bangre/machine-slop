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

export type Member = {
    id: string;
    name: string;
    email: string;
    role: "Owner" | "Admin" | "Member";
    status: "active" | "removed";
    is_you: boolean;
};

export type Project = { id: string; name: string; owner_id: string };

export type MemberOffboardingScenario = {
    members: Member[];
    projects: Project[];
    target_id: string;
    recipient_id: string;
    lookalike_id: string;
    recipient_lookalike_id: string;
};

const name_pairs: readonly (readonly [string, string])[] = [
    ["Priya Shah", "Priya Sharma"],
    ["Daniel Okafor", "Daniela Okafor"],
    ["Jonas Berg", "Jonas Bergström"],
    ["Aisha Karim", "Aisha Kareem"],
    ["Tomás Ortega", "Tomás Ortiz"],
    ["Mei Lin", "Mei Ling"],
];

const solo_names = [
    "Ruth Mensah",
    "Oskar Lind",
    "Farah Haddad",
    "Lukas Novak",
    "Grace Adeyemi",
    "Henrik Dahl",
];

const project_pool = [
    "Atlas migration",
    "Billing revamp",
    "Customer portal",
    "Data warehouse",
    "Design system",
    "Edge caching",
    "Field app",
    "Growth experiments",
    "Helpdesk bot",
    "Identity service",
    "Mobile checkout",
    "Partner API",
    "Pricing page",
    "Search relevance",
];

const domain = "harbourline.test";
const list_latency_ms = 200;
const action_latency_ms = 150;

function emailFor(name: string): string {
    return `${
        name.normalize("NFD").replace(/[̀-ͯ]/g, "").toLowerCase()
            .replaceAll(" ", ".")
    }@${domain}`;
}

export function memberOffboardingScenario(
    seed: number,
): MemberOffboardingScenario {
    const random = seededRandom(seed);
    const [target_pair, recipient_pair] = random.sample(name_pairs, 2) as [
        readonly [string, string],
        readonly [string, string],
    ];
    const target_flip = random.int(0, 1);
    const recipient_flip = random.int(0, 1);
    const target_name = target_pair[target_flip]!;
    const lookalike_name = target_pair[1 - target_flip]!;
    const recipient_name = recipient_pair[recipient_flip]!;
    const recipient_lookalike_name = recipient_pair[1 - recipient_flip]!;
    const names = random.shuffle([
        ...target_pair,
        ...recipient_pair,
        ...random.sample(solo_names, 3),
    ]);
    const members: Member[] = [
        {
            id: "m-100",
            name: "Sam Reyes",
            email: emailFor("Sam Reyes"),
            role: "Owner",
            status: "active",
            is_you: true,
        },
        ...names.map((name, index): Member => ({
            id: `m-${101 + index * 7}`,
            name,
            email: emailFor(name),
            role: random.next() < 0.3 ? "Admin" : "Member",
            status: "active",
            is_you: false,
        })),
    ];
    const idOf = (name: string) =>
        members.find((member) => member.name === name)!.id;
    const target_id = idOf(target_name);
    const recipient_id = idOf(recipient_name);
    const lookalike_id = idOf(lookalike_name);

    const project_names = random.sample(project_pool, 12);
    const target_count = random.int(2, 4);
    const owners = project_names.map((_, index) => {
        if (index < target_count) return target_id;
        if (index === target_count) return lookalike_id;
        if (index === target_count + 1) return recipient_id;
        let owner = random.pick(members).id;
        while (owner === target_id) owner = random.pick(members).id;
        return owner;
    });
    const projects = random.shuffle(
        project_names.map((name, index) => ({
            name,
            owner_id: owners[index]!,
        })),
    ).map((project, index) => ({ id: `p-${index + 1}`, ...project }));

    return {
        members,
        projects,
        target_id,
        recipient_id,
        lookalike_id,
        recipient_lookalike_id: idOf(recipient_lookalike_name),
    };
}

const page = renderPage({
    title: "Members",
    brand: "Harbourline · Workspace settings",
    style: `
  #projects-list { margin: 4px 0 0 18px; }
`,
    body: `
<main>
  <h1 id="heading">Members</h1>
  <p id="notice" class="notice" role="status" aria-live="polite"></p>
  <p id="loading" role="status">Loading members…</p>
  <table aria-labelledby="heading">
    <thead>
      <tr>
        <th scope="col">Name</th>
        <th scope="col">Email</th>
        <th scope="col">Role</th>
        <th scope="col">Projects owned</th>
        <th scope="col"><span class="visually-hidden">Actions</span></th>
      </tr>
    </thead>
    <tbody id="rows"></tbody>
  </table>
</main>
<dialog id="announcement" aria-labelledby="announcement-title">
  <h2 id="announcement-title"></h2>
  <p id="announcement-body"></p>
  <div class="actions"><button type="button" class="primary" id="announcement-dismiss">Got it</button></div>
</dialog>
<dialog id="remove" aria-labelledby="remove-title">
  <form id="remove-form">
    <h2 id="remove-title">Remove member</h2>
    <p id="remove-loading" role="status"></p>
    <div id="projects-section">
      <p id="projects-summary"></p>
      <ul id="projects-list"></ul>
      <fieldset>
        <legend id="recipient-legend">New owner of these projects</legend>
        <div id="recipients"></div>
      </fieldset>
    </div>
    <label for="confirm-email" id="confirm-label">Type the member's email address to confirm</label>
    <input type="text" id="confirm-email" name="confirm_email" autocomplete="off">
    <p id="remove-error" class="error" role="alert"></p>
    <div class="actions">
      <button type="button" id="remove-cancel">Cancel</button>
      <button type="submit" class="danger" id="remove-submit" disabled>Remove member</button>
    </div>
  </form>
</dialog>
`,
    script: `
let members = [];
let current_member = null;
let current_projects = [];
function renderMembers() {
    byId("rows").replaceChildren(...members.map((member) => h("tr", {},
        h("th", { scope: "row" }, member.name + (member.is_you ? " (you)" : "")),
        h("td", {}, member.email),
        h("td", {}, member.role),
        h("td", {}, String(member.project_count)),
        h("td", {}, member.is_you ? "" : h("button", { type: "button", "aria-label": "Remove " + member.name, onclick: () => openRemove(member) }, "Remove")))));
}
async function loadMembers() {
    byId("loading").hidden = false;
    const result = await api("GET", "/api/members");
    byId("loading").hidden = true;
    if (!result.ok) {
        byId("notice").textContent = "Members could not be loaded.";
        return;
    }
    members = result.data.members;
    renderMembers();
}
function updateSubmit() {
    const recipient_chosen = current_projects.length === 0 || new FormData(byId("remove-form")).get("transfer_to") !== null;
    const confirmed = byId("confirm-email").value.trim() === current_member.email;
    byId("remove-submit").disabled = !(recipient_chosen && confirmed);
}
async function openRemove(member) {
    current_member = member;
    current_projects = [];
    byId("remove-form").reset();
    byId("remove-title").textContent = "Remove " + member.name;
    byId("confirm-label").textContent = "Type " + member.email + " to confirm";
    byId("remove-error").textContent = "";
    byId("projects-section").hidden = true;
    byId("remove-submit").disabled = true;
    byId("remove-loading").textContent = "Loading projects…";
    byId("remove").showModal();
    const result = await api("GET", "/api/members/" + member.id + "/projects");
    byId("remove-loading").textContent = "";
    if (!result.ok) {
        byId("remove-error").textContent = "Projects could not be loaded.";
        return;
    }
    current_projects = result.data.projects;
    const first_name = member.name.split(" ")[0];
    if (current_projects.length > 0) {
        byId("projects-summary").textContent = first_name + " owns " + current_projects.length + " project" + (current_projects.length === 1 ? "" : "s") + ". Choose who takes them over.";
        byId("projects-list").replaceChildren(...current_projects.map((project) => h("li", {}, project.name)));
        byId("recipient-legend").textContent = "New owner of " + first_name + "'s projects";
        byId("recipients").replaceChildren(...members.filter((other) => other.id !== member.id).map((other) =>
            h("label", {}, h("input", { type: "radio", name: "transfer_to", value: other.id, onchange: updateSubmit }), other.name + " (" + other.email + ")")));
        byId("projects-section").hidden = false;
    }
    updateSubmit();
}
byId("confirm-email").addEventListener("input", updateSubmit);
byId("remove-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const transfer_to = new FormData(byId("remove-form")).get("transfer_to");
    byId("remove-submit").disabled = true;
    const result = await api("POST", "/api/members/" + current_member.id + "/remove", {
        transfer_to: current_projects.length > 0 ? transfer_to : null,
        confirm_email: byId("confirm-email").value.trim(),
    });
    if (!result.ok) {
        byId("remove-error").textContent = (result.data && result.data.error) || "The member could not be removed.";
        updateSubmit();
        return;
    }
    byId("remove").close();
    byId("notice").textContent = current_member.name + " was removed from the workspace.";
    await loadMembers();
});
byId("remove-cancel").addEventListener("click", () => byId("remove").close());
byId("announcement-dismiss").addEventListener("click", () => byId("announcement").close());
(async () => {
    const announcement = await api("GET", "/api/announcement");
    if (announcement.ok) {
        byId("announcement-title").textContent = announcement.data.title;
        byId("announcement-body").textContent = announcement.data.body;
        byId("announcement").showModal();
    }
    await loadMembers();
})();
`,
});

function openSite(scenario: MemberOffboardingScenario): WorkflowSite {
    const members = scenario.members.map((member) => ({ ...member }));
    const projects = scenario.projects.map((project) => ({ ...project }));
    const expected_members = new Map(
        scenario.members.map((member) => [
            member.id,
            member.id === scenario.target_id ? "removed" : "active",
        ]),
    );
    const expected_owners = new Map(
        scenario.projects.map((project) => [
            project.id,
            project.owner_id === scenario.target_id
                ? scenario.recipient_id
                : project.owner_id,
        ]),
    );
    const active = () => members.filter((member) => member.status === "active");
    const ownedBy = (id: string) =>
        projects.filter((project) => project.owner_id === id);

    return settledSite({
        async handle(request) {
            const { method, path } = routeOf(request);
            if (method === "GET" && path === "/") return htmlResponse(page);
            if (method === "GET" && path === "/api/announcement") {
                await pause(action_latency_ms);
                return Response.json({
                    title: "Scheduled maintenance",
                    body:
                        "Workspace exports are paused on Saturday 16 May 2026 from 02:00 to 04:00 UTC while storage is upgraded. Nothing else is affected.",
                });
            }
            if (method === "GET" && path === "/api/members") {
                await pause(list_latency_ms);
                return Response.json({
                    members: active().map((member) => ({
                        id: member.id,
                        name: member.name,
                        email: member.email,
                        role: member.role,
                        is_you: member.is_you,
                        project_count: ownedBy(member.id).length,
                    })),
                });
            }
            const member_route = path.match(
                /^\/api\/members\/([^/]+)\/(projects|remove)$/,
            );
            const member = members.find((entry) =>
                entry.id === member_route?.[1]
            );
            if (!member_route || !member || member.status !== "active") {
                return notFound();
            }
            if (method === "GET" && member_route[2] === "projects") {
                await pause(action_latency_ms);
                return Response.json({
                    projects: ownedBy(member.id).map(({ id, name }) => ({
                        id,
                        name,
                    })),
                });
            }
            if (method === "POST" && member_route[2] === "remove") {
                await pause(action_latency_ms);
                const body = await readObject(request);
                if (!body) return errorResponse(400, "Malformed request.");
                if (member.is_you) {
                    return errorResponse(400, "You cannot remove yourself.");
                }
                if (body.confirm_email !== member.email) {
                    return errorResponse(
                        400,
                        `Type ${member.email} exactly to confirm.`,
                        { confirm_email: "mismatch" },
                    );
                }
                const owned = ownedBy(member.id);
                const recipient = active().find((entry) =>
                    entry.id === body.transfer_to && entry.id !== member.id
                );
                if (owned.length > 0 && !recipient) {
                    return errorResponse(
                        400,
                        "Choose an active member to take over the projects.",
                        { transfer_to: "invalid" },
                    );
                }
                if (owned.length === 0 && body.transfer_to != null) {
                    return errorResponse(
                        400,
                        "This member owns no projects to transfer.",
                        { transfer_to: "unexpected" },
                    );
                }
                for (const project of owned) project.owner_id = recipient!.id;
                member.status = "removed";
                return Response.json({ removed: member.id });
            }
            return notFound();
        },
        outcome() {
            if (members.every((member) => member.status === "active")) {
                return "no member was removed";
            }
            return firstDifference(
                "member",
                expected_members,
                new Map(members.map((member) => [member.id, member.status])),
            ) ??
                firstDifference(
                    "project owner of",
                    expected_owners,
                    new Map(
                        projects.map((project) => [
                            project.id,
                            project.owner_id,
                        ]),
                    ),
                );
        },
    });
}

export const member_offboarding: Workflow = {
    id: "member-offboarding",
    covers: ["dialogs"],
    seeds: [21, 24, 27],
    open(seed) {
        const scenario = memberOffboardingScenario(seed);
        const nameOf = (id: string) =>
            scenario.members.find((member) => member.id === id)!.name;
        const prompt = `${
            nameOf(scenario.target_id)
        } is leaving the company. Please take them off our workspace, and make ${
            nameOf(scenario.recipient_id)
        } the owner of every project they currently own.`;
        return { prompt, site: openSite(scenario) };
    },
};
