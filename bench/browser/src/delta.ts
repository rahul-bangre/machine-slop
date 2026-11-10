const unchanged = "@@ unchanged";
const largest_aligned_region = 4_000_000;

type Hunk = { start: number; removed: number; added: string[] };

function alignedHunks(
    previous: string[],
    next: string[],
    offset: number,
): Hunk[] {
    const rows = previous.length;
    const columns = next.length;
    if (rows * columns > largest_aligned_region) {
        return [{ start: offset, removed: rows, added: next }];
    }
    const common = Array.from(
        { length: rows + 1 },
        () => new Uint32Array(columns + 1),
    );
    for (let row = rows - 1; row >= 0; row--) {
        for (let column = columns - 1; column >= 0; column--) {
            common[row]![column] = previous[row] === next[column]
                ? common[row + 1]![column + 1]! + 1
                : Math.max(
                    common[row + 1]![column]!,
                    common[row]![column + 1]!,
                );
        }
    }

    const hunks: Hunk[] = [];
    let open: Hunk | null = null;
    let row = 0;
    let column = 0;
    while (row < rows || column < columns) {
        if (row < rows && column < columns && previous[row] === next[column]) {
            open = null;
            row++;
            column++;
            continue;
        }
        if (!open) {
            open = { start: offset + row, removed: 0, added: [] };
            hunks.push(open);
        }
        const removes = column === columns ||
            (row < rows &&
                common[row + 1]![column]! >= common[row]![column + 1]!);
        if (removes) {
            open.removed++;
            row++;
        } else {
            open.added.push(next[column]!);
            column++;
        }
    }
    return hunks;
}

export function diffLines(previous_text: string, next_text: string): string {
    if (previous_text === next_text) return unchanged;
    const previous = previous_text.split("\n");
    const next = next_text.split("\n");

    let prefix = 0;
    while (
        prefix < previous.length && prefix < next.length &&
        previous[prefix] === next[prefix]
    ) prefix++;
    let suffix = 0;
    while (
        suffix < previous.length - prefix && suffix < next.length - prefix &&
        previous.at(-1 - suffix) === next.at(-1 - suffix)
    ) suffix++;

    return alignedHunks(
        previous.slice(prefix, previous.length - suffix),
        next.slice(prefix, next.length - suffix),
        prefix,
    ).map((hunk) =>
        [
            `@@ -${hunk.start + 1},${hunk.removed}`,
            ...hunk.added.map((line) => `+${line}`),
        ].join("\n")
    ).join("\n");
}

export function applyDelta(previous_text: string, delta: string): string {
    if (delta === unchanged) return previous_text;
    const previous = previous_text.split("\n");
    const rebuilt: string[] = [];
    let consumed = 0;
    for (const line of delta.split("\n")) {
        const header = line.match(/^@@ -(\d+),(\d+)$/);
        if (header) {
            const start = Number(header[1]) - 1;
            if (start < consumed || start > previous.length) {
                throw new Error(`delta hunk out of order or range: ${line}`);
            }
            rebuilt.push(...previous.slice(consumed, start));
            consumed = start + Number(header[2]);
        } else if (line.startsWith("+")) {
            rebuilt.push(line.slice(1));
        } else {
            throw new Error(`malformed delta line: ${line}`);
        }
    }
    if (consumed > previous.length) {
        throw new Error(
            "delta removes lines past the end of the previous state",
        );
    }
    rebuilt.push(...previous.slice(consumed));
    return rebuilt.join("\n");
}
