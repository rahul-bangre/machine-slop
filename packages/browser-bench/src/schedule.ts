export function armOrder<T>(arms: T[], round: number, taskIndex: number): T[] {
    const offset = (round + taskIndex) % arms.length;
    return [...arms.slice(offset), ...arms.slice(0, offset)];
}
