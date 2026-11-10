import type { AttemptRecord, FailureKind } from "./attempt.ts";

export type Spread = { median: number; min: number; max: number };

export function spread(values: number[]): Spread | null {
    if (values.length === 0) return null;
    const sorted = [...values].sort((left, right) => left - right);
    const middle = Math.floor(sorted.length / 2);
    const median = sorted.length % 2 === 1
        ? sorted[middle]!
        : (sorted[middle - 1]! + sorted[middle]!) / 2;
    return { median, min: sorted[0]!, max: sorted.at(-1)! };
}

export type Reliability = {
    runs: number;
    passedFirstAttempt: number;
    passedAfterRetries: number;
    failedAttempts: Partial<Record<FailureKind, number>>;
};

export function reliability(attempts: AttemptRecord[]): Reliability {
    const rounds = Map.groupBy(attempts, (attempt) => attempt.round);
    const failedAttempts: Partial<Record<FailureKind, number>> = {};
    for (const attempt of attempts) {
        if (!attempt.failure) continue;
        const { kind } = attempt.failure;
        failedAttempts[kind] = (failedAttempts[kind] ?? 0) + 1;
    }
    const outcomes = [...rounds.values()];
    return {
        runs: outcomes.length,
        passedFirstAttempt:
            outcomes.filter((round) =>
                round.some((attempt) =>
                    attempt.attempt === 1 && attempt.status === "passed"
                )
            ).length,
        passedAfterRetries:
            outcomes.filter((round) =>
                round.some((attempt) => attempt.status === "passed")
            ).length,
        failedAttempts,
    };
}

const sum = (values: number[]) =>
    values.reduce((total, value) => total + value, 0);

const metrics: {
    label: string;
    digits: number;
    read(attempt: AttemptRecord): number;
}[] = [
    {
        label: "startup ms",
        digits: 0,
        read: (attempt) => attempt.startup!.usablePageMs,
    },
    {
        label: "action ms",
        digits: 0,
        read: (attempt) => sum(attempt.steps.map((step) => step.actionMs)),
    },
    {
        label: "readiness ms",
        digits: 0,
        read: (attempt) => sum(attempt.steps.map((step) => step.readinessMs)),
    },
    {
        label: "observation ms",
        digits: 0,
        read: (attempt) => sum(attempt.steps.map((step) => step.observationMs)),
    },
    {
        label: "observation tokens",
        digits: 0,
        read: (attempt) =>
            sum(attempt.steps.map((step) => step.observationTokens)),
    },
    {
        label: "peak RSS sum MiB",
        digits: 0,
        read: (attempt) => attempt.peakRssSumBytes / 2 ** 20,
    },
    {
        label: "active CPU s",
        digits: 2,
        read: (attempt) =>
            sum(attempt.steps.map((step) => step.activeCpuSeconds)),
    },
    {
        label: "think CPU s",
        digits: 2,
        read: (attempt) =>
            sum(attempt.steps.map((step) => step.thinkCpuSeconds)),
    },
    { label: "total CPU s", digits: 2, read: (attempt) => attempt.cpuSeconds },
];

function table(header: string[], rows: string[][]): string {
    return [
        `| ${header.join(" | ")} |`,
        `| ${header.map(() => "---").join(" | ")} |`,
        ...rows.map((row) => `| ${row.join(" | ")} |`),
    ].join("\n");
}

function formatSpread(value: Spread | null, digits: number): string {
    if (!value) return "no passing run";
    const show = (number: number) => number.toFixed(digits);
    return `${show(value.median)} (${show(value.min)}–${show(value.max)})`;
}

export function renderSummary(
    attempts: AttemptRecord[],
    armNames: string[],
    taskIds: string[],
): string {
    const select = (arm: string, task: string) =>
        attempts.filter((attempt) =>
            attempt.arm === arm && attempt.task === task
        );
    const passing = (arm: string, task: string) =>
        select(arm, task).filter((attempt) => attempt.status === "passed");
    const baseline = armNames[0]!;
    const sections: string[] = [];

    sections.push(
        "## Reliability\n\nEvery attempt is counted, including failed ones and retries.\n\n" +
            table(
                [
                    "task",
                    "arm",
                    "runs",
                    "passed first attempt",
                    "passed after retries",
                    "failed attempts by kind",
                ],
                taskIds.flatMap((task) =>
                    armNames.map((arm) => {
                        const result = reliability(select(arm, task));
                        const failures = Object.entries(result.failedAttempts)
                            .map(([kind, count]) => `${kind} ${count}`)
                            .join(", ");
                        return [
                            task,
                            arm,
                            String(result.runs),
                            String(result.passedFirstAttempt),
                            String(result.passedAfterRetries),
                            failures || "none",
                        ];
                    })
                ),
            ),
    );

    sections.push(
        "## Cost per task\n\nMedian (min–max) over passing attempts. Step timings are summed over the task's steps.\n\n" +
            taskIds.map((task) =>
                `### ${task}\n\n` +
                table(
                    ["metric", ...armNames],
                    metrics.map((metric) => [
                        metric.label,
                        ...armNames.map((arm) =>
                            formatSpread(
                                spread(passing(arm, task).map(metric.read)),
                                metric.digits,
                            )
                        ),
                    ]),
                )
            ).join("\n\n"),
    );

    if (armNames.length > 1) {
        sections.push(
            `## Ratio to ${baseline}\n\nRatio of medians; below 1.00 is cheaper than ${baseline}.\n\n` +
                taskIds.map((task) =>
                    `### ${task}\n\n` +
                    table(
                        ["metric", ...armNames.slice(1)],
                        metrics.map((metric) => {
                            const reference = spread(
                                passing(baseline, task).map(metric.read),
                            );
                            return [
                                metric.label,
                                ...armNames.slice(1).map((arm) => {
                                    const value = spread(
                                        passing(arm, task).map(metric.read),
                                    );
                                    return value && reference &&
                                            reference.median > 0
                                        ? (value.median / reference.median)
                                            .toFixed(2)
                                        : "n/a";
                                }),
                            ];
                        }),
                    )
                ).join("\n\n"),
        );
    }
    return sections.join("\n\n") + "\n";
}
