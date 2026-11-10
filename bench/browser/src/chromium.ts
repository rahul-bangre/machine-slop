import { encodeHex } from "./hex.ts";
import pin from "../chromium.json" with { type: "json" };

type PlatformPin = { url: string; sha256: string; executable: string };

const package_root = new URL("../", import.meta.url);
const install_root = new URL(`.chromium/${pin.version}/`, package_root);

export const pinned_product = pin.product;

function platformPin(): PlatformPin {
    const key = `${Deno.build.os}-${Deno.build.arch}`;
    const platforms: Record<string, PlatformPin> = pin.platforms;
    const platform = platforms[key];
    if (!platform) {
        throw new Error(`no pinned Chromium for ${key}`);
    }
    return platform;
}

export function pinnedExecutable(): string {
    return decodeURIComponent(
        new URL(platformPin().executable, install_root).pathname,
    );
}

export async function isPinnedChromiumInstalled(): Promise<boolean> {
    try {
        return (await Deno.stat(pinnedExecutable())).isFile;
    } catch (error) {
        if (error instanceof Deno.errors.NotFound) return false;
        throw error;
    }
}

export async function installPinnedChromium(): Promise<string> {
    if (await isPinnedChromiumInstalled()) return pinnedExecutable();
    const platform = platformPin();
    await Deno.mkdir(install_root, { recursive: true });
    const archive = new URL("chromium.zip", install_root).pathname;

    const response = await fetch(platform.url);
    if (!response.ok || !response.body) {
        throw new Error(`download failed: ${response.status} ${platform.url}`);
    }
    await Deno.writeFile(archive, response.body);

    const digest = encodeHex(
        await crypto.subtle.digest("SHA-256", await Deno.readFile(archive)),
    );
    if (digest !== platform.sha256) {
        await Deno.remove(archive);
        throw new Error(
            `Chromium archive sha256 ${digest} does not match pin ${platform.sha256}`,
        );
    }

    const unzip = await new Deno.Command("unzip", {
        args: ["-q", "-o", archive, "-d", install_root.pathname],
        stdout: "null",
        stderr: "inherit",
    }).output();
    if (!unzip.success) throw new Error("unzip failed");
    await Deno.remove(archive);
    return pinnedExecutable();
}

if (import.meta.main) {
    console.log(await installPinnedChromium());
}
