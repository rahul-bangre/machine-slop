export const credential_variables = [
    "E2B_API_KEY",
    "E2B_ACCESS_TOKEN",
    "DAYTONA_API_KEY",
    "DAYTONA_JWT_TOKEN",
];

export class CredentialInArtifact extends Error {
    constructor(path: string, variable: string) {
        super(
            `refusing to write ${path}: it contains the value of ${variable}`,
        );
        this.name = "CredentialInArtifact";
    }
}

export function serializeArtifact(
    path: string,
    value: unknown,
    environment: Record<string, string> = Deno.env.toObject(),
): string {
    const text = JSON.stringify(value, null, 2) + "\n";
    for (const variable of credential_variables) {
        const secret = environment[variable];
        if (secret && text.includes(secret)) {
            throw new CredentialInArtifact(path, variable);
        }
    }
    return text;
}

export async function writeArtifact(
    path: string,
    value: unknown,
): Promise<void> {
    await Deno.writeTextFile(path, serializeArtifact(path, value));
}
