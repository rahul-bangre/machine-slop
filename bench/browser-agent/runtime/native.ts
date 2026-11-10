import type { AxNode } from "../../browser/src/ax.ts";
import type { Browser } from "../../browser/src/browser.ts";
import { TimeoutError, withDeadline } from "../../browser/src/cdp.ts";
import type {
    Action,
    Condition,
    Driver,
    State,
    Target,
    View,
} from "./driver.ts";

type Node = AxNode & { backendDOMNodeId?: number; aria_sort?: string };
type Operation = {
    epoch: number;
    deadline: number;
    timeout_ms: number;
    cancelled: boolean;
};
type Remote = {
    result: { value?: unknown; objectId?: string };
    exceptionDetails?: { text: string; exception?: { description?: string } };
};

class TargetAbsent extends Error {}

function role(node: Node): string {
    return node.role?.value === "DisclosureTriangle"
        ? "button"
        : String(node.role?.value ?? "none");
}

export async function connectNative(browser: Browser): Promise<Driver> {
    const driver = new Native(browser);
    await driver.prepare();
    return driver;
}

class Native implements Driver {
    private epoch = 0;
    private readonly issued = new Set<number>();
    private readonly detach: (() => void)[] = [];
    private closed = false;

    constructor(private readonly browser: Browser) {}

    async prepare(): Promise<void> {
        this.detach.push(
            this.browser.page.on("Page.frameNavigated", (params) => {
                const frame = params.frame as { parentId?: string };
                if (!frame.parentId) this.reset();
            }),
        );
        this.detach.push(
            this.browser.page.on("Page.loadEventFired", () => this.reset()),
        );
        await this.browser.page.send("Page.enable");
        await this.browser.page.send("Accessibility.enable");
    }

    async observe(view: View): Promise<State> {
        this.assertOpen();
        const epoch = this.epoch;
        const parent = view.mode === "scope" ? view.parent ?? 0 : 0;
        if (!Number.isInteger(parent) || parent < 0 || parent > 4) {
            throw new Error("scope parent must be 0 through 4");
        }
        const backend = view.mode === "scope"
            ? await this.ancestor(await this.target(view.target), parent)
            : undefined;
        const nodes = backend !== undefined
            ? await this.query(backend)
            : view.mode === "overview"
            ? await this.overview()
            : await this.tree();
        const root = backend === undefined
            ? undefined
            : nodes.find((node) => node.backendDOMNodeId === backend);
        if (backend !== undefined && !root) {
            throw new Error("scope is absent from current accessibility tree");
        }
        const modal = view.mode === "overview"
            ? nodes.find((node) =>
                !node.ignored &&
                node.properties?.some((property) =>
                    property.name === "modal" && property.value.value === true
                )
            )
            : undefined;
        const scoped = root ?? modal;
        const selected = nodes;
        await this.readSort(selected);
        this.assertEpoch(epoch);
        return {
            text: this.render(selected),
            scope: scoped
                ? `subtree:${this.issue(scoped.backendDOMNodeId!)}:complete`
                : view.mode === "overview"
                ? "document:overview:depth=2; table header and first 3 rows; omissions explicit"
                : "document:complete",
            format:
                "native-ax; fresh sample; refs expire on navigation/load; DOM aria-sort sampled separately; not atomic",
        };
    }

    async act(action: Action, timeout_ms: number): Promise<void> {
        this.assertOpen();
        const operation = {
            epoch: this.epoch,
            deadline: performance.now() + timeout_ms,
            timeout_ms,
            cancelled: false,
        };
        try {
            await withDeadline(
                "native action (outcome uncertain; action was not replayed)",
                timeout_ms,
                this.perform(action, operation),
            );
        } finally {
            operation.cancelled = true;
        }
    }

    wait(condition: Condition, timeout_ms: number): Promise<void> {
        this.assertOpen();
        return withDeadline(
            "native wait",
            timeout_ms,
            this.poll(condition, timeout_ms),
        );
    }

    private async poll(
        condition: Condition,
        timeout_ms: number,
    ): Promise<void> {
        const deadline = performance.now() + timeout_ms;
        while (performance.now() < deadline) {
            this.assertOpen();
            try {
                const holds = "text" in condition
                    ? await this.text(condition.text)
                    : await this.call(
                        await this.target(condition.target),
                        "function(property, expected) { return this.isConnected && this[property] === expected; }",
                        [
                            "value" in condition ? "value" : "checked",
                            "value" in condition
                                ? condition.value
                                : condition.checked,
                        ],
                    );
                if (holds === true) return;
            } catch (error) {
                if (!(error instanceof TargetAbsent)) throw error;
            }
            await new Promise((resolve) =>
                setTimeout(
                    resolve,
                    Math.min(20, Math.max(0, deadline - performance.now())),
                )
            );
        }
        throw new TimeoutError("native wait", timeout_ms);
    }

    async close(): Promise<void> {
        this.closed = true;
        for (const detach of this.detach) detach();
        this.detach.length = 0;
        this.issued.clear();
        await this.browser.page.send("Runtime.releaseObjectGroup", {
            objectGroup: "native-driver",
        });
    }

    private reset(): void {
        this.epoch++;
        this.issued.clear();
    }

    private assertOpen(): void {
        if (this.closed) throw new Error("native driver is closed");
    }

    private assertEpoch(epoch: number): void {
        if (epoch !== this.epoch) {
            throw new Error("document changed during operation");
        }
    }

    private issue(backend: number): string {
        this.issued.add(backend);
        return `${this.epoch}.${backend}`;
    }

    private backend(ref: string): number {
        const match = /^(\d+)\.(\d+)$/.exec(ref);
        if (
            !match || Number(match[1]) !== this.epoch ||
            !this.issued.has(Number(match[2]))
        ) throw new Error(`stale or unobserved ref: ${ref}`);
        return Number(match[2]);
    }

    private tree(depth?: number): Promise<Node[]> {
        return this.browser.page.send<{ nodes: Node[] }>(
            "Accessibility.getFullAXTree",
            depth === undefined ? {} : { depth },
        ).then((result) => result.nodes);
    }

    private async documentRoot(): Promise<number> {
        const { root } = await this.browser.page.send<
            { root: { backendNodeId: number } }
        >("DOM.getDocument", { depth: 0 });
        return root.backendNodeId;
    }

    private query(
        backend: number,
        filter: { role?: string; accessibleName?: string } = {},
    ): Promise<Node[]> {
        return this.browser.page.send<{ nodes: Node[] }>(
            "Accessibility.queryAXTree",
            { backendNodeId: backend, ...filter },
        ).then((result) => result.nodes);
    }

    private async target(target: Target): Promise<number> {
        if ("ref" in target) return this.backend(target.ref);
        const epoch = this.epoch;
        const backend = target.scope === undefined
            ? await this.documentRoot()
            : this.backend(target.scope);
        const roles = target.role === "button"
            ? ["button", "DisclosureTriangle"]
            : [target.role];
        const nodes = (await Promise.all(
            roles.map((role) =>
                this.query(backend, { role, accessibleName: target.name })
            ),
        )).flat();
        const matches = [
            ...new Map(
                nodes.filter((node) =>
                    !node.ignored && role(node) === target.role &&
                    node.name?.value === target.name &&
                    node.backendDOMNodeId !== undefined
                ).map((node) => [node.backendDOMNodeId, node]),
            ).values(),
        ];
        this.assertEpoch(epoch);
        if (matches.length === 0) {
            throw new TargetAbsent(
                "target requires exactly one match; found 0",
            );
        }
        if (matches.length !== 1) {
            throw new Error(
                `target requires exactly one match; found ${matches.length}`,
            );
        }
        return matches[0]!.backendDOMNodeId!;
    }

    private async call(
        backend: number,
        declaration: string,
        args: unknown[] = [],
        operation?: Operation,
    ): Promise<unknown> {
        if (operation) this.checkOperation(operation);
        const epoch = this.epoch;
        const { object } = await this.browser.page.send<
            { object: { objectId?: string } }
        >("DOM.resolveNode", {
            backendNodeId: backend,
            objectGroup: "native-driver",
        });
        if (!object.objectId) throw new Error("target is no longer available");
        try {
            this.assertEpoch(epoch);
            if (operation) this.checkOperation(operation);
            const result = await this.browser.page.send<Remote>(
                "Runtime.callFunctionOn",
                {
                    objectId: object.objectId,
                    functionDeclaration: declaration,
                    arguments: args.map((value) => ({ value })),
                    returnByValue: true,
                },
            );
            if (result.exceptionDetails) {
                throw new Error(
                    result.exceptionDetails.exception?.description ??
                        result.exceptionDetails.text,
                );
            }
            this.assertEpoch(epoch);
            return result.result.value;
        } finally {
            await this.browser.page.send("Runtime.releaseObject", {
                objectId: object.objectId,
            });
        }
    }

    private async ancestor(backend: number, parent: number): Promise<number> {
        if (parent === 0) return backend;
        const { object } = await this.browser.page.send<
            { object: { objectId: string } }
        >("DOM.resolveNode", {
            backendNodeId: backend,
            objectGroup: "native-driver",
        });
        try {
            const result = await this.browser.page.send<Remote>(
                "Runtime.callFunctionOn",
                {
                    objectId: object.objectId,
                    functionDeclaration:
                        "function(count) { const path = [this]; for (const index of Array.from({length: count}, (_, i) => i)) { const next = path[index].parentElement; if (!next) throw new Error('scope exceeds ancestors'); path.push(next); } return path[count]; }",
                    arguments: [{ value: parent }],
                    objectGroup: "native-driver",
                },
            );
            if (result.exceptionDetails || !result.result.objectId) {
                throw new Error("scope exceeds ancestors");
            }
            try {
                const { node } = await this.browser.page.send<
                    { node: { backendNodeId: number } }
                >("DOM.describeNode", { objectId: result.result.objectId });
                return node.backendNodeId;
            } finally {
                await this.browser.page.send("Runtime.releaseObject", {
                    objectId: result.result.objectId,
                });
            }
        } finally {
            await this.browser.page.send("Runtime.releaseObject", {
                objectId: object.objectId,
            });
        }
    }

    private async overview(): Promise<Node[]> {
        const backend = await this.documentRoot();
        for (const modal_role of ["dialog", "alertdialog"]) {
            const { nodes: dialogs } = await this.browser.page.send<
                { nodes: Node[] }
            >("Accessibility.queryAXTree", {
                backendNodeId: backend,
                role: modal_role,
            });
            const modal = dialogs.find((node) =>
                !node.ignored &&
                node.properties?.some((property) =>
                    property.name === "modal" && property.value.value === true
                )
            );
            if (modal?.backendDOMNodeId !== undefined) {
                return this.query(modal.backendDOMNodeId);
            }
        }
        const nodes = await this.tree(2);
        const by_id = new Map(nodes.map((node) => [node.nodeId, node]));
        const children = async (node: Node): Promise<Node[]> => {
            const { nodes: found } = await this.browser.page.send<
                { nodes: Node[] }
            >("Accessibility.getChildAXNodes", { id: node.nodeId });
            for (const child of found) by_id.set(child.nodeId, child);
            return (node.childIds ?? []).flatMap((id) =>
                by_id.has(id) ? [by_id.get(id)!] : []
            );
        };
        for (
            const table of nodes.filter((node) =>
                ["table", "grid", "treegrid"].includes(role(node))
            )
        ) {
            const rows: Node[] = [];
            const collect = async (
                node: Node,
                depth: number,
            ): Promise<void> => {
                for (const child of await children(node)) {
                    if (role(child) === "row") rows.push(child);
                    else if (
                        depth > 0 &&
                        ["rowgroup", "generic", "none"].includes(role(child))
                    ) await collect(child, depth - 1);
                }
            };
            await collect(table, 2);
            const count = { data: 0 };
            for (const row of rows) {
                if (count.data >= 3) {
                    for (const omitted of subtree([...by_id.values()], row)) {
                        by_id.delete(omitted.nodeId);
                    }
                    continue;
                }
                const cells = await children(row);
                if (!cells.some((cell) => role(cell) === "columnheader")) {
                    count.data++;
                }
            }
        }
        return [...by_id.values()];
    }

    private async readSort(nodes: Node[]): Promise<void> {
        for (
            const header of nodes.filter((node) =>
                role(node) === "columnheader" &&
                node.backendDOMNodeId !== undefined
            )
        ) {
            const { node } = await this.browser.page.send<
                { node: { attributes?: string[] } }
            >("DOM.describeNode", { backendNodeId: header.backendDOMNodeId });
            const attributes = node.attributes ?? [];
            const index = attributes.findIndex((value, index) =>
                index % 2 === 0 && value === "aria-sort"
            );
            if (index >= 0) header.aria_sort = attributes[index + 1]!;
        }
    }

    private render(nodes: Node[]): string {
        const by_id = new Map(nodes.map((node) => [node.nodeId, node]));
        const lines: string[] = [];
        const visited = new Set<string>();
        const visit = (node: Node, depth: number): void => {
            if (visited.has(node.nodeId)) return;
            visited.add(node.nodeId);
            const parts = [role(node)];
            if (node.name?.value !== undefined) {
                parts.push(JSON.stringify(node.name.value));
            }
            if (node.backendDOMNodeId !== undefined) {
                parts.push(`ref=${this.issue(node.backendDOMNodeId)}`);
            }
            if (node.ignored) parts.push("ignored");
            if (node.role?.value === "DisclosureTriangle") {
                parts.push("native-role=DisclosureTriangle");
            }
            if (node.value?.value !== undefined) {
                parts.push(`value=${JSON.stringify(node.value.value)}`);
            }
            for (const property of node.properties ?? []) {
                parts.push(
                    `${property.name}=${
                        JSON.stringify(property.value.value ?? property.value)
                    }`,
                );
            }
            if (node.aria_sort !== undefined) {
                parts.push(`aria-sort=${JSON.stringify(node.aria_sort)}`);
            }
            const missing =
                (node.childIds ?? []).filter((id) => !by_id.has(id)).length;
            if (missing) {
                parts.push(
                    `[omitted children=${missing}; expand ref for complete subtree]`,
                );
            }
            lines.push("  ".repeat(depth) + parts.join(" "));
            for (const id of node.childIds ?? []) {
                const child = by_id.get(id);
                if (child) visit(child, depth + 1);
            }
        };
        for (const node of nodes) {
            if (node.parentId === undefined || !by_id.has(node.parentId)) {
                visit(node, 0);
            }
        }
        return lines.join("\n");
    }

    private async text(text: string): Promise<boolean> {
        const result = await this.browser.page.send<
            { result: { value: boolean } }
        >("Runtime.evaluate", {
            expression: `document.body?.innerText.includes(${
                JSON.stringify(text)
            }) ?? false`,
            returnByValue: true,
        });
        return result.result.value;
    }

    private checkOperation(operation: Operation): void {
        this.assertOpen();
        this.assertEpoch(operation.epoch);
        if (operation.cancelled || performance.now() >= operation.deadline) {
            throw new TimeoutError(
                "native action (outcome uncertain; action was not replayed)",
                operation.timeout_ms,
            );
        }
    }

    private input(
        operation: Operation,
        method: string,
        params: Record<string, unknown>,
    ): Promise<unknown> {
        this.checkOperation(operation);
        return this.browser.page.send(method, params);
    }

    private async perform(
        action: Action,
        operation: Operation,
    ): Promise<void> {
        if (action.kind === "press") {
            await this.input(operation, "Input.dispatchKeyEvent", {
                type: "keyDown",
                key: "Enter",
                code: "Enter",
                windowsVirtualKeyCode: 13,
                text: "\r",
            });
            await this.input(operation, "Input.dispatchKeyEvent", {
                type: "keyUp",
                key: "Enter",
                code: "Enter",
                windowsVirtualKeyCode: 13,
            });
            return;
        }
        if (action.kind === "scroll") {
            await this.input(operation, "Input.dispatchMouseEvent", {
                type: "mouseWheel",
                x: 640,
                y: 400,
                deltaX: 0,
                deltaY: action.dy,
            });
            return;
        }
        const backend = await this.target(action.target);
        const point = await this.call(
            backend,
            `function(fill) {
            if (!this.isConnected) throw new Error('target detached');
            if (this.matches(':disabled') || this.closest('[aria-disabled="true"]')) throw new Error('target disabled');
            this.scrollIntoView({block:'center',inline:'center',behavior:'instant'});
            const box = this.getBoundingClientRect();
            if (!box.width || !box.height) throw new Error('target has no visible area');
            const x = Math.max(0, box.left) + (Math.min(innerWidth, box.right) - Math.max(0, box.left)) / 2;
            const y = Math.max(0, box.top) + (Math.min(innerHeight, box.bottom) - Math.max(0, box.top)) / 2;
            const hit = document.elementFromPoint(x,y);
            if (!hit || !this.contains(hit)) throw new Error('target obscured');
            if (fill && ((!this.matches('input,textarea') && !this.isContentEditable) || this.readOnly)) throw new Error('target is not editable');
            return {x,y};
        }`,
            [action.kind === "fill"],
            operation,
        ) as { x: number; y: number };
        await this.input(operation, "Input.dispatchMouseEvent", {
            type: "mouseMoved",
            ...point,
        });
        await this.input(operation, "Input.dispatchMouseEvent", {
            type: "mousePressed",
            ...point,
            button: "left",
            clickCount: 1,
        });
        await this.input(operation, "Input.dispatchMouseEvent", {
            type: "mouseReleased",
            ...point,
            button: "left",
            clickCount: 1,
        });
        if (action.kind === "fill") {
            await this.call(
                backend,
                `function() {
                if (document.activeElement !== this) throw new Error('target did not receive focus');
                if (this.matches('input,textarea')) this.select();
                else { const range = document.createRange(); range.selectNodeContents(this); const selection = getSelection(); selection.removeAllRanges(); selection.addRange(range); }
            }`,
                [],
                operation,
            );
            await this.input(operation, "Input.insertText", {
                text: action.text,
            });
        }
    }
}

function subtree(nodes: Node[], root: Node): Node[] {
    const by_id = new Map(nodes.map((node) => [node.nodeId, node]));
    const selected: Node[] = [];
    const visit = (node: Node): void => {
        selected.push(node);
        for (const id of node.childIds ?? []) {
            const child = by_id.get(id);
            if (child) visit(child);
        }
    };
    visit(by_id.get(root.nodeId) ?? root);
    return selected;
}
