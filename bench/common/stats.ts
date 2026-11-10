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

export function formatSpread(
    value: Spread | null,
    digits: number,
    when_empty: string,
): string {
    if (!value) return when_empty;
    const show = (number: number) => number.toFixed(digits);
    return `${show(value.median)} (${show(value.min)}–${show(value.max)})`;
}

export const sum = (values: number[]) =>
    values.reduce((total, value) => total + value, 0);

export function markdownTable(header: string[], rows: string[][]): string {
    return [
        `| ${header.join(" | ")} |`,
        `| ${header.map(() => "---").join(" | ")} |`,
        ...rows.map((row) => `| ${row.join(" | ")} |`),
    ].join("\n");
}

export function wholeNumber(
    flag: string,
    value: string,
    minimum: number,
): number {
    const number = Number(value);
    if (!Number.isInteger(number) || number < minimum) {
        throw new RangeError(
            `--${flag} must be a whole number of at least ${minimum}`,
        );
    }
    return number;
}
