export type Task = {
    id: string;
    prompt: string;
    files: Record<string, string>;
    checker: string;
    solution: Record<string, string>;
};

const tasks_root = new URL("../tasks/", import.meta.url);

async function readTree(
    directory: URL,
    prefix = "",
): Promise<Record<string, string>> {
    const files: Record<string, string> = {};
    for await (const entry of Deno.readDir(directory)) {
        const path = `${prefix}${entry.name}`;
        if (entry.isDirectory) {
            Object.assign(
                files,
                await readTree(
                    new URL(`${entry.name}/`, directory),
                    `${path}/`,
                ),
            );
        } else {
            files[path] = await Deno.readTextFile(
                new URL(entry.name, directory),
            );
        }
    }
    return files;
}

export async function loadTasks(): Promise<Task[]> {
    const ids: string[] = [];
    for await (const entry of Deno.readDir(tasks_root)) {
        if (entry.isDirectory) ids.push(entry.name);
    }
    return await Promise.all(
        ids.sort().map(async (id) => {
            const root = new URL(`${id}/`, tasks_root);
            return {
                id,
                prompt: await Deno.readTextFile(new URL("prompt.md", root)),
                files: await readTree(new URL("files/", root)),
                checker: await Deno.readTextFile(new URL("check.py", root)),
                solution: await readTree(new URL("solution/", root)),
            };
        }),
    );
}
