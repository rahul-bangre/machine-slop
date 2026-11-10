export function armOrder<T>(arms: T[], round: number, task_index: number): T[] {
    const offset = (round + task_index) % arms.length;
    return [...arms.slice(offset), ...arms.slice(0, offset)];
}
