# Machines

`machined` creates and destroys machines on a Linux host with `/dev/kvm`. Each
machine is a Firecracker microVM running
[`agentd`](../../apps/agentd/README.md).

```ts
import { Host } from "@machine-slop/sdk";

const host = new Host("http://127.0.0.1:4001");
const { machine, info } = await host.createMachine({
  vcpus: 2,
  memory_mib: 512,
});
const process = await machine.startProcess({
  program: "/bin/sh",
  args: ["-c", "uname -a"],
});
console.log(info.boot_ms, await process.wait());
await machine.destroy();
```

| Request                 | SDK                             | Result                                                                  |
| ----------------------- | ------------------------------- | ----------------------------------------------------------------------- |
| `POST /machines`        | `host.createMachine(...)`       | Boots a machine; 201 with `MachineInfo` once `agentd` answers `/health` |
| `GET /machines`         | `host.machines()`               | Lists machines by id                                                    |
| `GET /machines/{id}`    | `machine.info()`                | `MachineInfo`                                                           |
| `PATCH /machines/{id}`  | `machine.setAllowedHosts(list)` | Replaces the machine's `allowed_hosts`; `MachineInfo`                   |
| `DELETE /machines/{id}` | `machine.destroy()`             | Kills the VM and removes its files; 204                                 |
| `/machines/{id}/...`    | `host.machine(id)`              | Forwarded to that machine's `agentd`, upgrades included                 |

`HostedMachine` is a `Machine`, so processes, files and ports work the same as
against a local `agentd`. `GET /health` returns `ok`.

| Field               | Default | Range     |
| ------------------- | ------- | --------- |
| `vcpus`             | 2       | 1–32      |
| `memory_mib`        | 512     | 128–32768 |
| `idle_timeout_secs` | 600     | 1–86400   |
| `allowed_hosts`     | `["*"]` | see below |

An invalid request returns 422, a full host or a shutting-down `machined` 503, a
failed boot 500 with the console tail, an unknown machine 404, and an
unreachable `agentd` 502. A host runs at most 16 machines. Admission also
reserves requested guest memory plus 64 MiB per VM for host overhead, before
launching. The default memory budget is Linux `MemAvailable` at daemon startup
minus the greater of 512 MiB or 10%. Set `SLOP_MEMORY_MIB` to a smaller total
budget for machines and their overhead. Each create also checks current
`MemAvailable`, leaving 512 MiB for the host. Memory exhaustion returns 503
before creating VM files. Reservations include boots and exited VMs and release
after destruction or failed-boot cleanup, even if a caller retains the machine
handle.

This is admission control for a dedicated Linux host, not an operating-system
memory limit. Other workloads, cgroup limits and growing host-side buffers need
room outside the budget; use a smaller explicit budget when sharing the host.

`boot_ms` runs from the start of launch until `agentd` is healthy. A create or
delete finishes even if its client disconnects. Creates are not idempotent:
after a lost create response, find the machine with `host.machines()` instead of
retrying. A machine whose VM exits on its own stays listed with state `exited`,
and keeps its slot, until it is deleted or expires.

A machine expires after `idle_timeout_secs` without an open request to its
`agentd`, and is then destroyed as if deleted. The clock restarts when the last
open request to `/machines/{id}/...` ends, so a long `wait()`, an event stream
or an open port tunnel keeps the machine alive. Requests to `machined` itself,
such as `GET /machines/{id}`, and egress traffic do not. To keep an unattended
machine, call `machine.health()` within each timeout.

## Machine

The root filesystem is one read-only ext4 image shared by every machine. Writes
go to an overlay on the machine's own sparse 8 GiB scratch disk, so a machine
starts with the image's contents and nothing another machine wrote. `agentd`
runs as root under `tini` in `/workspace` and listens on vsock port 3000. The
image includes `curl`, `git`, `nodejs`, `python3` with `pip` and `venv`, and
`build-essential`.

## Egress

A machine has no network interface. Its only way out is an HTTPS proxy that
`machined` runs for it. By default it may reach any public host over HTTPS. A
create request can narrow that list, or turn egress off with an empty one:

```ts
await host.createMachine({ allowed_hosts: ["pypi.org", "*.pythonhosted.org"] });
await host.createMachine({ allowed_hosts: [] });
```

| Pattern         | Matches                                           |
| --------------- | ------------------------------------------------- |
| `example.com`   | `example.com` only                                |
| `*.example.com` | any subdomain of `example.com`, not `example.com` |
| `*`             | every host                                        |
| `93.184.215.14` | that IP address, written as a host                |

Patterns are case-insensitive and `MachineInfo.allowed_hosts` echoes them
normalized. A malformed pattern returns 422, as does a name whose last label is
numeric, such as `8.0.1`, which a resolver would read as an address.

`setAllowedHosts` decides every later connection and closes open tunnels to
hosts the new list no longer allows. Closing follows the response
asynchronously, so a few bytes may still pass.

Inside the machine, `HTTPS_PROXY` and `HTTP_PROXY` (and their lowercase forms)
point at `127.0.0.1:3128`, where `socat` forwards each connection over vsock to
`machined`. `machined` accepts only `CONNECT host:443`, checks the host against
the list, resolves it itself, and connects only to public unicast addresses, so
even `*` cannot reach the host's loopback, private networks, link-local metadata
endpoints or the host's own public address. curl, git over HTTPS, pip and
Python's `urllib` honour the proxy variables.

A refusal is an HTTP response to the `CONNECT` with a plain-text reason:

- 403: a port other than 443, a host outside the list, no public address, or the
  host itself
- 405: anything but `CONNECT`, plain HTTP included
- 400: a malformed request
- 502: the name does not resolve, or no address accepts within 3 seconds
- 504: resolving and connecting take longer than 10 seconds

Each machine may hold 256 open proxy connections and 4 concurrent name lookups,
so one machine cannot starve another. `machined` raises its soft open-file limit
to cover every machine's budget at start and refuses to start if the hard limit
is too low.

The list limits the name a machine asks for and the address `machined` connects
to. It does not inspect the TLS server name or HTTP `Host` inside the tunnel.
Egress leaves from the host's own address, so under `*` a machine reaches
anything the host can reach at a public address, including services that trust
the host, and can send anything it holds to any public HTTPS host. Give machines
that hold secrets and run untrusted instructions a narrower list or `[]`.

Not supported: plain HTTP, ports other than 443, DNS from inside the machine,
UDP, and tools that ignore the proxy variables, such as Node's built-in `fetch`
and git over SSH.

## Host

`machined <listen address> <image dir> <state dir>`

The image dir holds `firecracker`, `vmlinux` and `rootfs.ext4`, built by
`vm/image/build.sh <image dir> <agentd binary>`. The script pins Firecracker and
the guest kernel by hash and Debian by snapshot date, and rebuilds the root
filesystem only when its inputs change.

`machined` holds a lock on its state dir. On start it kills Firecracker
processes left by an earlier `machined` that died and removes their files. On
SIGTERM or SIGINT it closes admission, destroys machines concurrently, and waits
for admitted boots and deletions to finish before exiting.

Both daemons emit JSON logs to stderr, with `RUST_LOG` controlling filtering
(default `info`). Machine lifecycle events include machine IDs, boot duration
and requested resources; cleanup and egress listener failures include errors.
`agentd` process-start events are available at `debug`.

`GET /metrics` exposes Prometheus text. `machined_machines_active` and
`machined_memory_reserved_mib` include boots and pending cleanup, while
`machined_memory_budget_mib` reports the admission budget.
`machined_creates_total{result="success"|"failure"}` counts validated create
attempts through their terminal outcome, including capacity rejection, even if
the client disconnects. These counters reset at daemon restart. `agentd` exposes
`agentd_processes_running` and `agentd_processes_retained`; the latter includes
finished records kept for replay.

CI runs the live machine contracts on a Linux KVM runner, serially to bound
memory use, and runs the ordinary checks on Linux and macOS.

Firecracker runs without its jailer, and the API has no authentication. The host
is trusted.
