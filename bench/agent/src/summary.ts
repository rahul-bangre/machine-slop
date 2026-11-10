import {
    formatSpread,
    markdownTable,
    spread,
    sum,
} from "../../common/stats.ts";
import type { AttemptRecord, FailureKind } from "./attempt.ts";

type Measured = AttemptRecord & {
    agent: NonNullable<AttemptRecord["agent"]>;
};

const metrics: {
    label: string;
    digits: number;
    read(attempt: Measured): number;
}[] = [
    {
        label: "sandbox create ms",
        digits: 0,
        read: (attempt) => attempt.create_ms ?? 0,
    },
    {
        label: "task file setup ms",
        digits: 0,
        read: (attempt) => attempt.setup_ms ?? 0,
    },
    { label: "model turns", digits: 0, read: (attempt) => attempt.agent.turns },
    {
        label: "commands",
        digits: 0,
        read: (attempt) => attempt.agent.commands.length,
    },
    {
        label: "sandbox ms per command",
        digits: 0,
        read: (attempt) =>
            attempt.agent.commands.length === 0
                ? 0
                : attempt.agent.sandbox_ms / attempt.agent.commands.length,
    },
    {
        label: "sandbox ms in commands",
        digits: 0,
        read: (attempt) => attempt.agent.sandbox_ms,
    },
    {
        label: "model ms",
        digits: 0,
        read: (attempt) => attempt.agent.model_ms,
    },
    {
        label: "input tokens",
        digits: 0,
        read: (attempt) =>
            attempt.agent.input_tokens + attempt.agent.cache_read_input_tokens +
            attempt.agent.cache_creation_input_tokens,
    },
    {
        label: "output tokens",
        digits: 0,
        read: (attempt) => attempt.agent.output_tokens,
    },
    { label: "wall ms", digits: 0, read: (attempt) => attempt.wall_ms },
    {
        label: "sandbox share of wall %",
        digits: 1,
        read: (attempt) =>
            100 *
            ((attempt.create_ms ?? 0) + (attempt.setup_ms ?? 0) +
                attempt.agent.sandbox_ms + (attempt.check_ms ?? 0) +
                (attempt.close_ms ?? 0)) /
            attempt.wall_ms,
    },
];

export function failureCounts(
    attempts: AttemptRecord[],
): Partial<Record<FailureKind, number>> {
    const counts: Partial<Record<FailureKind, number>> = {};
    for (const attempt of attempts) {
        if (!attempt.failure) continue;
        const { kind } = attempt.failure;
        counts[kind] = (counts[kind] ?? 0) + 1;
    }
    return counts;
}

export function renderSummary(
    attempts: AttemptRecord[],
    sandbox_names: string[],
    task_ids: string[],
): string {
    const select = (sandbox: string, task: string) =>
        attempts.filter((attempt) =>
            attempt.sandbox === sandbox && attempt.task === task
        );
    const finished = (sandbox: string, task: string) =>
        select(sandbox, task).filter((attempt): attempt is Measured =>
            attempt.agent?.stop === "finished"
        );

    const reliability =
        "## Results\n\nEvery attempt is counted. A pass means the task's checker script exited 0 inside the sandbox.\n\n" +
        markdownTable(
            ["task", "sandbox", "attempts", "passed", "failures by kind"],
            task_ids.flatMap((task) =>
                sandbox_names.map((sandbox) => {
                    const selected = select(sandbox, task);
                    const failures = Object.entries(failureCounts(selected))
                        .map(([kind, count]) => `${kind} ${count}`).join(", ");
                    return [
                        task,
                        sandbox,
                        String(selected.length),
                        String(sum(selected.map((attempt) =>
                            attempt.status === "passed" ? 1 : 0
                        ))),
                        failures || "none",
                    ];
                })
            ),
        );

    const cost =
        "## Cost per task\n\nMedian (min–max) over attempts where the model finished, whether or not the checker passed.\n\n" +
        task_ids.map((task) =>
            `### ${task}\n\n` +
            markdownTable(
                ["metric", ...sandbox_names],
                metrics.map((metric) => [
                    metric.label,
                    ...sandbox_names.map((sandbox) =>
                        formatSpread(
                            spread(finished(sandbox, task).map(metric.read)),
                            metric.digits,
                            "no finished run",
                        )
                    ),
                ]),
            )
        ).join("\n\n");

    return `${reliability}\n\n${cost}\n`;
}
