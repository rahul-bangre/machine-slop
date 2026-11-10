import chromium from "../../browser/chromium.json" with { type: "json" };
import { encodeHex } from "../../browser/src/hex.ts";

export type Resources = { vcpus: number; memory_mib: number };

export type Environment = {
    dockerfile: string;
    resources: Resources;
    chromium: { version: string; executable: string };
    deno: { version: string };
};

const base_image =
    "docker.io/library/debian:trixie-20261027-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4";
const debian_snapshot = "20261103T000000Z";
const deno_linux_x86_64_sha256: Record<string, string> = {
    "2.9.7": "c6527f24f4b16031d3ae4fa9f658d5f11534c8d84ce7dc8502420280919c3490",
};

const chromium_deb_deps_on_trixie = [
    "fonts-liberation",
    "libasound2t64",
    "libatk-bridge2.0-0t64",
    "libatk1.0-0t64",
    "libatspi2.0-0t64",
    "libcairo2",
    "libcups2t64",
    "libcurl4t64",
    "libdbus-1-3",
    "libexpat1",
    "libgbm1",
    "libglib2.0-0t64",
    "libgtk-3-0t64",
    "libnspr4",
    "libnss3",
    "libpango-1.0-0",
    "libudev1",
    "libvulkan1",
    "libx11-6",
    "libxcb1",
    "libxcomposite1",
    "libxdamage1",
    "libxext6",
    "libxfixes3",
    "libxkbcommon0",
    "libxrandr2",
    "wget",
    "xdg-utils",
];
const tool_packages = ["ca-certificates", "curl", "procps", "unzip"];

export const chromium_executable = "/usr/local/bin/chromium";

async function pinnedDenoVersion(): Promise<string> {
    const tool_versions = await Deno.readTextFile(
        new URL("../../../.tool-versions", import.meta.url),
    );
    const version = tool_versions.match(/^deno (\S+)$/m)?.[1];
    if (!version) throw new Error(".tool-versions does not pin deno");
    return version;
}

function renderDockerfile(deno_version: string): string {
    const deno_sha256 = deno_linux_x86_64_sha256[deno_version];
    if (!deno_sha256) {
        throw new Error(`no pinned archive hash for Deno ${deno_version}`);
    }
    const chromium_pin = chromium.platforms["linux-x86_64"];
    const deno_url =
        `https://github.com/denoland/deno/releases/download/v${deno_version}/deno-x86_64-unknown-linux-gnu.zip`;
    const snapshot = `http://snapshot.debian.org/archive`;
    return [
        `FROM ${base_image}`,
        "ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8 TZ=UTC",
        [
            "rm -f /etc/apt/sources.list.d/debian.sources /etc/apt/sources.list",
            `echo 'deb ${snapshot}/debian/${debian_snapshot} trixie main' > /etc/apt/sources.list.d/snapshot.list`,
            `echo 'deb ${snapshot}/debian/${debian_snapshot} trixie-updates main' >> /etc/apt/sources.list.d/snapshot.list`,
            `echo 'deb ${snapshot}/debian-security/${debian_snapshot} trixie-security main' >> /etc/apt/sources.list.d/snapshot.list`,
            `echo 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/99snapshot`,
            "apt-get update",
            `apt-get install -y --no-install-recommends ${
                [...tool_packages, ...chromium_deb_deps_on_trixie].join(" ")
            }`,
            "rm -rf /var/lib/apt/lists/*",
        ].join(" && ").replace(/^/, "RUN "),
        [
            `curl -fsSL -o /tmp/chromium.zip ${chromium_pin.url}`,
            `echo '${chromium_pin.sha256}  /tmp/chromium.zip' | sha256sum -c -`,
            "unzip -q /tmp/chromium.zip -d /opt/chromium",
            "rm /tmp/chromium.zip",
            `ln -s /opt/chromium/${chromium_pin.executable} ${chromium_executable}`,
        ].join(" && ").replace(/^/, "RUN "),
        [
            `curl -fsSL -o /tmp/deno.zip ${deno_url}`,
            `echo '${deno_sha256}  /tmp/deno.zip' | sha256sum -c -`,
            "unzip -q /tmp/deno.zip -d /usr/local/bin",
            "rm /tmp/deno.zip",
            "chmod 0755 /usr/local/bin/deno",
        ].join(" && ").replace(/^/, "RUN "),
        `RUN ${chromium_executable} --version && deno --version`,
        "",
    ].join("\n");
}

export async function browserEnvironment(): Promise<Environment> {
    const deno_version = await pinnedDenoVersion();
    return {
        dockerfile: renderDockerfile(deno_version),
        resources: { vcpus: 2, memory_mib: 4096 },
        chromium: {
            version: chromium.version,
            executable: chromium_executable,
        },
        deno: { version: deno_version },
    };
}

export async function sha256(text: string): Promise<string> {
    return encodeHex(
        await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text)),
    );
}
