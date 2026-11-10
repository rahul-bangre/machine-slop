type AxValue = { value?: unknown };

export type AxNode = {
    nodeId: string;
    ignored?: boolean;
    role?: AxValue;
    name?: AxValue;
    value?: AxValue;
    properties?: { name: string; value: AxValue }[];
    parentId?: string;
    childIds?: string[];
};

const transparent_roles = new Set(["none", "generic"]);
const dropped_roles = new Set(["InlineTextBox", "LineBreak"]);
const reported_states = new Set([
    "checked",
    "disabled",
    "expanded",
    "focused",
    "level",
    "pressed",
    "required",
    "selected",
]);

export function renderAxTree(nodes: AxNode[]): string {
    const by_id = new Map(nodes.map((node) => [node.nodeId, node]));
    const lines: string[] = [];

    const visit = (node: AxNode, depth: number, ancestor_name: string) => {
        const role = String(node.role?.value ?? "");
        if (dropped_roles.has(role)) return;
        const name = String(node.name?.value ?? "").trim();
        const is_transparent = node.ignored ||
            (transparent_roles.has(role) && name === "") ||
            (role === "StaticText" && ancestor_name.includes(name));
        if (!is_transparent) {
            lines.push("  ".repeat(depth) + describe(node, role, name));
        }
        for (const child_id of node.childIds ?? []) {
            const child = by_id.get(child_id);
            if (!child) continue;
            visit(
                child,
                is_transparent ? depth : depth + 1,
                is_transparent ? ancestor_name : name,
            );
        }
    };

    for (const node of nodes) {
        if (node.parentId === undefined) visit(node, 0, "");
    }
    return lines.join("\n");
}

function describe(node: AxNode, role: string, name: string): string {
    const parts = [role === "StaticText" ? "text" : role];
    if (name) parts.push(JSON.stringify(name));
    const value = node.value?.value;
    if (value !== undefined && value !== "") {
        parts.push(`value=${JSON.stringify(value)}`);
    }
    for (const property of node.properties ?? []) {
        const state = property.value.value;
        if (!reported_states.has(property.name) || state === false) continue;
        parts.push(
            state === true ? property.name : `${property.name}=${state}`,
        );
    }
    return parts.join(" ");
}
