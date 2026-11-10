import type { Action, Condition, Target, View } from "./driver.ts";

export type Command = {
    actions: Action[];
    wait?: Condition;
    view: View;
    timeout_ms: number;
};

function object(value: unknown): Record<string, unknown> {
    if (!value || typeof value !== "object" || Array.isArray(value)) {
        throw new Error("expected an object");
    }
    return value as Record<string, unknown>;
}

function text(value: unknown): string {
    if (typeof value !== "string" || value.length > 16_384) {
        throw new Error("expected text of at most 16384 characters");
    }
    return value;
}

function target(value: unknown): Target {
    const source = object(value);
    if (source.ref !== undefined) return { ref: text(source.ref) };
    return {
        role: text(source.role),
        name: text(source.name),
        ...(source.scope === undefined ? {} : { scope: text(source.scope) }),
    };
}

function action(value: unknown): Action {
    const source = object(value);
    if (source.kind === "click") {
        return { kind: "click", target: target(source.target) };
    }
    if (source.kind === "fill") {
        return {
            kind: "fill",
            target: target(source.target),
            text: text(source.text),
        };
    }
    if (source.kind === "press" && source.key === "Enter") {
        return { kind: "press", key: "Enter" };
    }
    if (
        source.kind === "scroll" && typeof source.dy === "number" &&
        Number.isFinite(source.dy) && Math.abs(source.dy) <= 10_000
    ) return { kind: "scroll", dy: source.dy };
    throw new Error(
        "unknown action; choose click, fill, press Enter, or scroll",
    );
}

function condition(value: unknown): Condition {
    const source = object(value);
    if (source.text !== undefined) return { text: text(source.text) };
    if (source.value !== undefined) {
        return { target: target(source.target), value: text(source.value) };
    }
    if (typeof source.checked === "boolean") {
        return { target: target(source.target), checked: source.checked };
    }
    throw new Error("wait needs text, or target with value/checked");
}

function view(value: unknown): View {
    if (value === undefined) return { mode: "overview" };
    const source = object(value);
    if (source.mode === "full" || source.mode === "overview") {
        return { mode: source.mode };
    }
    if (source.mode === "scope") {
        const parent = source.parent ?? 0;
        if (
            typeof parent !== "number" || !Number.isInteger(parent) ||
            parent < 0 || parent > 4
        ) {
            throw new Error("scope parent must be 0 through 4");
        }
        return { mode: "scope", target: target(source.target), parent };
    }
    throw new Error("view mode must be overview, full, or scope");
}

export function parseCommand(value: unknown): Command {
    const source = object(value);
    const actions = source.actions ?? [];
    if (!Array.isArray(actions) || actions.length > 16) {
        throw new Error("actions must be an array of at most 16 actions");
    }
    const timeout_ms = source.timeout_ms ?? 5_000;
    if (
        typeof timeout_ms !== "number" || !Number.isInteger(timeout_ms) ||
        timeout_ms < 1 || timeout_ms > 10_000
    ) {
        throw new Error("timeout_ms must be 1 through 10000");
    }
    return {
        actions: actions.map(action),
        ...(source.wait === undefined ? {} : { wait: condition(source.wait) }),
        view: view(source.view),
        timeout_ms,
    };
}
