# Browser environments

Builds the same browser environment on E2B and on Daytona, and checks that a
sandbox started from it is what was pinned.

```sh
export E2B_API_KEY=...
deno task env build e2b
deno task env smoke e2b

export DAYTONA_API_KEY=...
deno task env build daytona
deno task env smoke daytona
```

## What is pinned

`src/environment.ts` renders one Dockerfile, and both providers build from that
text:

- base image: Debian trixie slim, by digest
- Debian packages: from `snapshot.debian.org` at a fixed timestamp
- Chromium: Chrome for Testing at the version, URL and archive hash in
  `bench/browser/chromium.json`, checked with `sha256sum -c` during the build
- Deno: the version in `.tool-versions`, with its release archive hash
- resources: 2 vCPUs and 4 GiB of memory, set on the E2B template and on the
  Daytona snapshot

The guests are x86_64; no other architecture is pinned.

## What is recorded

`env build` writes `images/<provider>.json`:

- `image.ref`: what sandboxes are created from. For E2B, the template name and a
  tag naming this build; for Daytona, the snapshot name
- `image.id`: the E2B template ID or the Daytona snapshot ID
- `image.details`: the E2B build ID, or the Daytona image name
- `input_sha256`: a digest of the Dockerfile, the resources and the provider's
  build settings

`env smoke` refuses a record whose `input_sha256` no longer matches what the
current pins would build, so a changed pin cannot be smoke-tested against an old
image. Rebuild and commit the new record.

## What the smoke test checks

It starts one sandbox from the recorded image, labelled with a fresh run ID,
uploads `guest/observe.ts` and `guest/fixture.html`, and runs the observer with
the image's own Deno. The observer only reports what it sees; the host judges
it. A run passes when:

- the sandbox reports the recorded template or snapshot ID
- the provider reports 2 vCPUs and 4096 MiB
- inside the guest, the tightest of `nproc` and every cgroup `cpu.max` up to the
  root is 2, and the tightest of `MemTotal` and every cgroup `memory.max` is
  between 90% and 100% of 4096 MiB. A microVM shows its limits through its
  kernel; a container shows the host's and is limited by cgroups
- `deno` and `chromium --version` are the pinned versions
- headless Chromium loads the fixture from a server on 127.0.0.1 inside the
  sandbox, runs its script, and reports the pinned major version. The sandbox
  has no network access
- no Chromium process is left once the browser exits (after a 5 second grace
  period; anything left is killed and reported), and the profile directory is
  gone
- every sandbox labelled with the run ID was destroyed

Chromium runs with `--no-sandbox` on both providers: the sandbox is the
isolation boundary, and the same flags keep the two runs comparable.

The smoke test destroys every sandbox carrying its run label, so a sandbox whose
create response was lost is destroyed too. A create the provider has not yet
listed when the run ends is left to the provider: E2B kills it after 15 minutes,
and Daytona stops it after 15 idle minutes and deletes it because it is
ephemeral. Templates and snapshots are kept; they are what the records point at.

The report is written to `bench-results/env-<timestamp>/<provider>.json`.

## Credentials

API keys are read from the environment only, and are never passed into a
sandbox. Every record and report is refused before it is written if it would
contain the value of `E2B_API_KEY`, `E2B_ACCESS_TOKEN`, `DAYTONA_API_KEY` or
`DAYTONA_JWT_TOKEN`.
