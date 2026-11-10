export type Target = { ref: string } | {
    role: string;
    name: string;
    scope?: string;
};

export type Action =
    | { kind: "click"; target: Target }
    | { kind: "fill"; target: Target; text: string }
    | { kind: "press"; key: "Enter" }
    | { kind: "scroll"; dy: number };

export type View =
    | { mode: "overview" }
    | { mode: "full" }
    | { mode: "scope"; target: Target; parent?: number };

export type Condition = { text: string } | { target: Target; value: string } | {
    target: Target;
    checked: boolean;
};

export type State = { text: string; scope: string; format: string };

export interface Driver {
    observe(view: View): Promise<State>;
    act(action: Action, timeout_ms: number): Promise<void>;
    wait(condition: Condition, timeout_ms: number): Promise<void>;
    close(): Promise<void>;
}
