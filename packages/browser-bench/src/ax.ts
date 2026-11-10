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

const transparentRoles = new Set(["none", "generic"]);
const droppedRoles = new Set(["InlineTextBox", "LineBreak"]);
const reportedStates = new Set([
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
    const byId = new Map(nodes.map((node) => [node.nodeId, node]));
    const lines: string[] = [];

    const visit = (node: AxNode, depth: number, ancestorName: string) => {
        const role = String(node.role?.value ?? "");
        if (droppedRoles.has(role)) return;
        const name = String(node.name?.value ?? "").trim();
        const isTransparent = node.ignored ||
            (transparentRoles.has(role) && name === "") ||
            (role === "StaticText" && ancestorName.includes(name));
        if (!isTransparent) {
            lines.push("  ".repeat(depth) + describe(node, role, name));
        }
        for (const childId of node.childIds ?? []) {
            const child = byId.get(childId);
            if (!child) continue;
            visit(
                child,
                isTransparent ? depth : depth + 1,
                isTransparent ? ancestorName : name,
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
        if (!reportedStates.has(property.name) || state === false) continue;
        parts.push(
            state === true ? property.name : `${property.name}=${state}`,
        );
    }
    return parts.join(" ");
}
