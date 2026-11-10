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
- `image.details`: the E2B build ID and tag, or the registry image Daytona built
  the snapshot into
- `input_sha256`: a digest of the Dockerfile, the resources and the provider's
  build settings

`env smoke` refuses a record whose `input_sha256` no longer matches what the
current pins would build, so a changed pin cannot be smoke-tested against an old
image. Rebuild and commit the new record.

Image names carry the first 12 hex digits of `input_sha256`, so building again
with unchanged inputs is safe. On E2B it makes a new build and moves the tag to
it, and the new record names that build. On Daytona, snapshot names are unique:
an active snapshot under the name is recorded as it is, an inactive one is
activated, and a failed one is deleted and built again. A build whose record was
never written, for example because the process died, is recovered by running the
build again.

A sandbox is only used if it runs exactly the recorded build. On E2B every build
of a template shares the template ID and a sandbox does not report its build, so
the tag must still point at the recorded build ID when the sandbox is created.
On Daytona the sandbox's snapshot must have the recorded snapshot ID, which
changes if the snapshot is deleted and rebuilt under the same name.

## What the smoke test checks

It starts one sandbox from the recorded image, labelled with a fresh run ID,
uploads `guest/observe.ts` and `guest/fixture.html`, and runs the observer with
the image's own Deno. The observer only reports what it sees; the host judges
it. A run passes when:

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
- every sandbox the run created was destroyed, confirmed by looking each one up
  by ID

Chromium runs with `--no-sandbox` on both providers: the sandbox is the
isolation boundary, and the same flags keep the two runs comparable. The
observer runs as `user` on E2B, whose Dockerfile conversion adds that user, and
as root on Daytona.

The smoke test destroys every sandbox carrying its run label, so a sandbox whose
create response was lost is destroyed too, and the sandbox it started even if
the label listing does not show it yet or the listing fails (which fails the
run). Listings lag: Daytona's has been seen reporting a sandbox as running after
it was deleted. So the test does not list again to confirm cleanup; it looks up
each destroyed sandbox by ID until the provider reports it gone. A create the
provider has not listed when the run ends, and whose response was lost, is left
to the provider: E2B kills it after 15 minutes, and Daytona stops it after 15
idle minutes and deletes it because it is ephemeral. Templates and snapshots are
kept; they are what the records point at.

On the recorded images, an E2B sandbox is a microVM whose kernel sees 2 CPUs and
3803 MiB. A Daytona sandbox is a container that sees all of the host's CPUs,
held to 2 CPUs and 4096 MiB by its cgroup.

The report is written to `bench-results/env-<timestamp>/<provider>.json`.

## Credentials

API keys are read from the environment only, and are never passed into a
sandbox. Every record and report is refused before it is written if it would
contain the value of `E2B_API_KEY`, `E2B_ACCESS_TOKEN`, `DAYTONA_API_KEY` or
`DAYTONA_JWT_TOKEN`.
