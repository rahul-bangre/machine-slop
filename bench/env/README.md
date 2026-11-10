# Browser environments

Builds the same browser environment on E2B and Daytona, and checks that a
sandbox started from it matches the pins.

```sh
export E2B_API_KEY=...
deno task env build e2b
deno task env smoke e2b

export DAYTONA_API_KEY=...
deno task env build daytona
deno task env smoke daytona
```

## Pins

`src/environment.ts` renders one Dockerfile that both providers build, and the
resources both request:

- Debian trixie slim by digest, with packages from `snapshot.debian.org`
- Chromium from `bench/browser/chromium.json`, checked by hash
- Deno at the version in `.tool-versions`, checked by hash
- 2 vCPUs and 4 GiB

## Records

`env build` writes `images/<provider>.json` with the template or snapshot to
create sandboxes from, its ID, and a digest of the build inputs. `env smoke`
refuses a record whose digest no longer matches the pins, or whose template or
snapshot no longer points at the recorded build: rebuild and commit the new
record. Daytona reuses a snapshot built from unchanged inputs; E2B builds again
under the same tag, which invalidates other checkouts' records until they pull
the new one.

## Smoke test

Starts one sandbox from the recorded build, without network access, and passes
when:

- the provider and the guest's kernel and cgroups both show 2 vCPUs and about 4
  GiB
- Deno and Chromium are the pinned versions
- headless Chromium loads a fixture served inside the sandbox
- no Chromium process or profile is left behind
- every sandbox the run created is confirmed destroyed

The report is written to `bench-results/env-<timestamp>/<provider>.json`, or
under `--out <dir>`.

## Credentials

API keys are read from the environment and never passed into a sandbox. A record
or report that would contain one is not written.
