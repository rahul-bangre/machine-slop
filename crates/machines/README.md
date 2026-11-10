# Machines

`machined` runs machines on a Linux host with `/dev/kvm`. Each machine is a
Firecracker microVM running [`agentd`](../../apps/agentd/README.md). A machine
can be paused to disk and resumed with its memory intact, forked while it runs,
and snapshotted to start any number of new machines from the same moment.

```ts
import { Host } from "@machine-slop/sdk";

const host = new Host("http://127.0.0.1:4001", { apiKey: "..." });
const { machine, info } = await host.createMachine();
const waiting = await machine.startProcess({
  program: "/bin/sh",
  args: ["-c", "read line; echo $line"],
  keep_stdin_open: true,
});
console.log(info.boot_ms);

const fork = await machine.fork();
await fork.machine.process(waiting.id).writeStdin(0, "from the fork\n");

const snapshot = await machine.snapshot();
const options = await Promise.all(
  [1, 2, 3].map(() => host.createMachine({ snapshot_id: snapshot.id })),
);
await host.deleteSnapshot(snapshot.id);

await machine.pause();
await machine.resume();
await machine.process(waiting.id).writeStdin(0, "from the original\n");

for (const m of [machine, fork.machine, ...options.map((o) => o.machine)]) {
  await m.destroy();
}
```

The fork and the three machines from the snapshot each have their own copy of
the waiting shell. Deleting the snapshot leaves those machines running, and the
original machine's shell is still waiting after the pause and resume.

| Request                      | SDK                                   | Result                                                         |
| ---------------------------- | ------------------------------------- | -------------------------------------------------------------- |
| `POST /machines`             | `host.createMachine(...)`             | Starts a machine; 201 with `MachineInfo` once `agentd` answers |
| `GET /machines`              | `host.machines()`                     | Lists machines by id                                           |
| `GET /machines/{id}`         | `machine.info()`                      | `MachineInfo`                                                  |
| `PATCH /machines/{id}`       | `machine.setAllowedHosts(list)`       | Replaces the machine's `allowed_hosts`; `MachineInfo`          |
| `POST /machines/{id}/pause`  | `machine.pause()`                     | Saves the machine and stops its VM; `MachineInfo`              |
| `POST /machines/{id}/resume` | `machine.resume()`                    | Restores the machine once `agentd` answers; `MachineInfo`      |
| `POST /machines/{id}/fork`   | `machine.fork()`                      | A new machine from this one's current state; 201 with its info |
| `DELETE /machines/{id}`      | `machine.destroy()`                   | Kills the VM and removes its files, saved state included; 204  |
| `/machines/{id}/...`         | `host.machine(id)`                    | Forwarded to that machine's `agentd`, upgrades included        |
| `POST /snapshots`            | `machine.snapshot()`                  | Records the machine's current state; 201 with `SnapshotInfo`   |
| `GET /snapshots`             | `host.snapshots()`                    | Lists snapshots by id                                          |
| `GET /snapshots/{id}`        | `host.snapshot(id)`                   | `SnapshotInfo`                                                 |
| `DELETE /snapshots/{id}`     | `host.deleteSnapshot(id)`             | Removes the snapshot's files; 204                              |
| `POST /machines`             | `host.createMachine({ snapshot_id })` | Starts a new machine from a snapshot; 201 with `MachineInfo`   |
| `GET /usage`                 | `host.usage()`                        | The tenant's quotas and recorded usage; `TenantUsage`          |

`HostedMachine` is a `Machine`, so processes, files and ports work the same as
against a local `agentd`. `GET /health` returns `ok`. Every other request needs
an API key as `Authorization: Bearer <key>`, and acts as the key's tenant.

| Field               | Default | Range     |
| ------------------- | ------- | --------- |
| `vcpus`             | 2       | 1–32      |
| `memory_mib`        | 512     | 128–32768 |
| `idle_timeout_secs` | 600     | 1–86400   |
| `allowed_hosts`     | `["*"]` | see below |
| `snapshot_id`       | none    | see below |

A machine is `running`, `paused` or `exited`. A missing or unknown API key
returns 401, an invalid request 422, an unknown machine or snapshot 404, a
request that does not apply to the machine's state 409, a request beyond the
tenant's quotas or create rate 429, a host with no room for another VM or a
shutting-down `machined` 503, a failed boot 500 with the console tail, a failed
save or restore 500, and an unreachable `agentd` 502. Forwarding to a paused
machine is a 409; pausing, resuming, forking or snapshotting an exited one is
a 409. An exited machine can only be deleted or left to expire. A machine
becomes exited when its VM exits on its own, when a resume fails after its
checkpoint was given up, and when `machined` stops without saving it.

At most 16 VMs run at once, and each running VM reserves its guest memory plus
64 MiB of host overhead from a memory budget before it launches. The budget is
Linux `MemAvailable` when `machined` starts, minus the greater of 512 MiB or
10%; set `SLOP_MEMORY_MIB` to a smaller total. Each admission also checks the
current `MemAvailable`, leaving 512 MiB for the host. Paused and exited machines
run no VM and reserve neither a place nor memory, so any number can be parked;
they and snapshots are limited only by disk. A create, fork or resume that does
not fit returns 503 and changes nothing: no files are created, a resume leaves
the machine paused, and a fork is refused before its source is touched. A
machine larger than the whole budget is refused at once.

This is admission control for a dedicated Linux host, not an operating-system
memory limit. Other workloads, cgroup limits and growing host-side buffers need
room outside the budget; use a smaller explicit budget when sharing the host.

`boot_ms` is how long the request that created the machine took until `agentd`
was healthy. A create, delete, pause, resume, fork or snapshot finishes even if
its client disconnects. A delete that fails to remove a machine's or snapshot's
record returns 500 and leaves it listed; retry it.

## Retries

A create, fork or snapshot that carries an `Idempotency-Key` header can be
retried after a lost response. The SDK takes the key as the last argument of
`createMachine`, `fork` and `snapshot`. A key is 1 to 255 visible ASCII
characters and belongs to the tenant that sent it.

The key is written into the machine's or snapshot's record in the same step that
makes it exist, so a request either made its result with the key or made
nothing. A request with a key that already made a machine or snapshot returns
it, as it is now, with the original status and without checking quotas or the
create rate. Requests with the same key run one at a time: a retry that arrives
while the first attempt is still running waits for it. A failed attempt makes
nothing, so its retry tries again. Reusing a key for a different request, such
as another body, another source machine or another kind of request, returns 422.

A key refers to its result for as long as that machine or snapshot exists,
across `machined` restarts. Deleting or expiring a keyed result also takes the
key's turn, so a retry waits for it and then finds the result still listed if
the delete failed. Once the result is gone, the key makes a new one. A request
without a key is not retry-safe; after a lost response, find its result with
`host.machines()` or `host.snapshots()`.

A running or exited machine expires after `idle_timeout_secs` without an open
request to its `agentd`, and is then destroyed as if deleted. The clock restarts
when the last open request to `/machines/{id}/...` ends, so a long `wait()`, an
event stream or an open port tunnel keeps the machine alive, and when a pause,
resume, fork or snapshot of it ends. Other requests to `machined` itself, such
as `GET /machines/{id}`, and egress traffic do not. A paused machine never
expires, so pausing is how to keep a machine without keeping it busy; to keep a
running machine unattended, call `machine.health()` within each timeout. After a
`machined` restart an exited machine's clock starts again. An expiry that cannot
remove the machine's record leaves it listed and tries again a timeout later.

## Speed

Measured with 512 MiB machines on a 2-vCPU host with an ext4 state dir:

| Request                           | Time   | Dominated by                             |
| --------------------------------- | ------ | ---------------------------------------- |
| Create, default size              | ~10 ms | Recording a machine that already booted  |
| Create, other sizes or in a burst | ~1 s   | Booting the guest                        |
| Create from a snapshot            | ~60 ms | Copying the disk; memory loads on demand |
| Resume                            | ~50 ms | Starting Firecracker; memory on demand   |
| Pause                             | ~1 s   | Writing and syncing all of guest memory  |
| Fork or snapshot, running machine | ~1 s   | Writing memory; the machine stops for it |
| Snapshot, paused machine          | ~15 ms | Copying the disk                         |

`machined` keeps two default-size machines booted, or `SLOP_SPARE_MACHINES` of
them, and a create takes one instead of booting. Spares are replaced in the
background and never listed. They use places and memory that nothing else is
using: when a VM that was asked for needs either, a spare is shut down to make
room, after it finishes booting if it has not yet. A create of another size, or
a default-size one when no spare is ready and there is room, boots.

Saving memory costs time in proportion to the machine's memory size, and
restoring it does not. To explore several continuations of one state, take one
snapshot and create each machine from it rather than forking repeatedly, so the
save is paid once. Disks are reflinked where the state dir's filesystem supports
it (XFS, Btrfs) and copied sparsely, in proportion to what the machine wrote,
where it does not (ext4).

## Pause and resume

A successful pause means the machine's memory, device state and disk are
complete and synced on the host, and its VM is gone. The machine keeps its id
and survives a `machined` restart or a host reboot.

A successful resume means the same guest is running again and `agentd` answers:
processes, their memory, `agentd`'s process registry and files in memory-backed
filesystems are as they were. Every connection into the machine is gone.
Requests that were in flight when it paused, such as an event stream or a
`wait()`, end with an error; reconnect with `machine.process(id)` and
`events(last_seq + 1)`. The guest's clock does not advance while the machine is
paused.

Pausing a paused machine or resuming a running one returns its `MachineInfo` and
does nothing, so either can be retried after a lost response. Requests that
change one machine run one at a time: concurrent resumes restore one VM, and a
delete that overlaps a resume waits for it, then deletes the machine.

When a pause or resume fails, the 500 body ends with what the machine is now,
and `machine.info()` agrees:

- A failed pause leaves the machine running, with no checkpoint, although its
  connections may have been reset. If the saved state could not be withdrawn,
  the guest stays stopped and the machine is paused; if the VM cannot continue
  for another reason, it is exited.
- A resume that fails before the guest runs leaves the machine paused, and can
  be retried.
- A resume that fails after that, including `agentd` not answering within 30
  seconds, leaves the machine exited. The guest may already have written to its
  disk, so the saved memory no longer matches it and is not kept.

## Fork and snapshots

A fork is a new machine in the state its source is in at that moment, with a new
id and its own memory and disk. A snapshot is that state kept as an immutable
restore point: memory, device state, disk, root image and the Firecracker binary
that saved it. A machine created from a snapshot gets the snapshot's vCPUs and
memory, which the request may not override.

Forking or snapshotting a running machine stops it only while its state is
saved; it then carries on with its processes, but its connections are reset as
with a pause. If saving fails the machine carries on and nothing is created; if
it cannot carry on, it is exited and the 500 body says so. A paused machine is
forked or snapshotted from its checkpoint and stays paused.

A machine, its forks, a snapshot, and the machines created from the snapshot
share nothing that is ever written. Whatever one does later, the others and the
snapshot do not change. They all continue from the same instant: the same
processes, hostname, clock, and random values already held in program memory. On
x86_64 the guest kernel reseeds its random number generator on restore; on
aarch64 the pinned kernel does not.

A snapshot does not depend on its source machine, which can be deleted, and it
survives a `machined` restart. Deleting a snapshot means no more machines can be
created from it; machines already created from it keep running and can still be
paused, resumed and forked. A delete waits for creates that are reading the
snapshot.

Saved state restores on the host that made it. Firecracker does not support
restoring on a different CPU model, and does not promise it across host kernel
versions; a restore that Firecracker rejects fails and leaves the snapshot, or
the paused machine, as it was.

## Machine

The root filesystem is one read-only ext4 image shared by every machine. Writes
go to an overlay on the machine's own sparse 8 GiB scratch disk, so a machine
starts with the image's contents and nothing another machine wrote. `agentd`
runs as root under `tini` in `/workspace` and listens on vsock port 3000. The
image includes `curl`, `git`, `nodejs`, `python3` with `pip` and `venv`,
`build-essential`, Deno, and a headless Chromium at `/usr/local/bin/chromium`.

On x86_64 the browser is the Chrome for Testing build pinned in
`bench/browser/chromium.json`, the same one the E2B and Daytona benchmark images
use. Chrome for Testing has no Linux arm64 build, so on aarch64 it is Debian's
`chromium` from the image's snapshot. Run it as
`chromium --headless=new --no-sandbox` in a machine with at least 1024 MiB: at
the 512 MiB default its renderer cannot reserve memory and page loads never
finish. It loads pages served inside the machine directly and allowed HTTPS
sites through egress, with no proxy flags. To drive it from outside, start it
with `--remote-debugging-port` and reach that port with `machine.forward`.

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
asynchronously, so a few bytes may still pass. It runs one at a time with the
machine's other changes, so it waits for a pause, resume, fork or snapshot in
flight, and a fork or snapshot that starts after it never carries the old list.

A machine keeps its list while paused and across a `machined` restart, and
`setAllowedHosts` works on a paused machine. A fork starts with its source's
list and idle timeout. A snapshot records its source's list as
`SnapshotInfo.allowed_hosts`, and a machine created from it gets that list
unless the request gives its own; its idle timeout is the request's or the
default. A spare has no egress until a create takes it and gives it that
machine's list. Open tunnels are among the connections a pause, fork or snapshot
resets.

Inside the machine, `HTTPS_PROXY` and `HTTP_PROXY` (and their lowercase forms)
point at `127.0.0.1:3128`, where `socat` forwards each connection over vsock to
`machined`. `machined` accepts only `CONNECT host:443`, checks the host against
the list, resolves it itself, and connects only to public unicast addresses, so
even `*` cannot reach the host's loopback, private networks, link-local metadata
endpoints or the host's own public address. curl, git over HTTPS, pip, Python's
`urllib` and Chromium honour the proxy variables.

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

## Tenants

Every machine and snapshot belongs to the tenant that created it; a fork belongs
to its source's tenant. A tenant sees and changes only its own: another tenant's
machine, snapshot or port is a 404, and so is creating from another tenant's
snapshot. `machined` reads its tenants at start from a JSON file:

```json
{
  "tenants": [
    {
      "id": "acme",
      "api_key_sha256": ["<hex SHA-256 of each API key>"],
      "quotas": {
        "running_machines": 8,
        "memory_mib": 16384,
        "disk_gib": 200,
        "creates_per_second": 2
      }
    }
  ]
}
```

The file holds digests, not keys; `printf %s "$key" | sha256sum` makes one. A
tenant may have several keys, so a key can be rotated by adding the new one
before removing the old. An id is 1 to 64 letters, digits, `-` or `_`. A tenant
removed from the file keeps its machines, snapshots and usage, but no key
reaches them; they still count toward its usage until an operator deletes them.

| Quota                | Counts                                                               | Checked by             |
| -------------------- | -------------------------------------------------------------------- | ---------------------- |
| `running_machines`   | VMs running for the tenant                                           | create, fork, resume   |
| `memory_mib`         | guest memory of those VMs                                            | create, fork, resume   |
| `disk_gib`           | each machine's 8 GiB scratch disk plus its memory, and each snapshot | create, fork, snapshot |
| `creates_per_second` | creates and forks, with bursts of up to one second's worth           | create, fork           |

A running VM holds its machine and memory quota until its process exits, so a
pause gives them back and a resume takes them again; paused and exited machines
hold only disk. Disk is provisioned rather than measured: a machine counts the
most it can store, whether or not it is paused, and a snapshot counts what it
stored, `SnapshotInfo.stored_bytes`. A request over a quota or the rate returns
429 before anything starts and changes nothing; a rate refusal carries
`Retry-After`, and a `creates_per_second` of 0 refuses every create and fork.
`GET /usage` reports the quotas.

## Usage

`machined` meters each tenant's

- `vcpu_ms`: vCPU-milliseconds, each machine's vCPUs times how long it runs:
  from when a create or fork records it, or a resume finds `agentd` answering,
  until its VM process exits. A boot, restore or resume that fails is not
  metered, nor is a paused machine;
- `memory_mib_ms`: MiB-milliseconds of guest memory over the same time;
- `snapshot_mib_ms`: MiB-milliseconds of `stored_bytes` for each snapshot, from
  its creation until it is deleted, including while `machined` is down.

Divide by 1000 for seconds and by 1024 for GiB. Usage is recorded in
`usage.json` in the state dir every 5 seconds and at shutdown, and `GET /usage`
reports only what has been recorded, with `as_of_unix_ms`, so it never goes
down, even across a crash. A crash loses at most the VM time of the 5 seconds
before it; it is never counted twice. Metering uses the host's wall clock. For
every tenant, configured or not, `usage.json` is the export for billing.

## Isolation

Every VM runs under Firecracker's `jailer`, which `machined` runs as root:

- a chroot holding only that VM's files: hard links to its disks, its kernel or
  the saved state it restores from, and its sockets;
- a user and group of its own: `machined` keeps one id per running-VM place,
  from `SLOP_FIRST_JAIL_UID` (default 1000000000) up, so no two running VMs
  share one, and an id is reused only after its VM has exited;
- a cgroup under `/sys/fs/cgroup/machine-slop-<state dir device>-<inode>` with
  `memory.max` at the admitted reservation (guest memory plus 64 MiB),
  `memory.swap.max` 0, `cpu.max` at its vCPUs and `pids.max` at its vCPUs plus
  32;
- Firecracker's own seccomp filters.

The machine's scratch disk is writable inside its jail; the root image, kernel
and saved state are read-only to it. Because the jail's root belongs to the VM's
user, `machined` treats everything in it as the VM's to change: it connects to
the VM's API and `agentd` sockets only when the process serving them is that
VM's Firecracker, and takes saved memory and device state only as regular files
owned by the VM's user, opened without following links. It then makes them owned
by root and read-only, and links that same file out of the jail, before any
other machine or snapshot shares it. The state dir's `machines` and `snapshots`
are readable only by root. `machined` removes the API key from requests it
forwards to a machine, so the key never enters a guest.

## Host

`machined <listen address> <image dir> <state dir> <tenants file>`

`machined` must run as root on cgroup v2 with the `cpu`, `memory` and `pids`
controllers available. Machine and snapshot records written before tenants
existed name no tenant; `machined` logs them as unreadable and leaves their
directories in place, so remove the old state dir when upgrading. The image dir
holds `jailer`, `firecracker`, `vmlinux` and `rootfs.ext4`, built by
`vm/image/build.sh <image dir> <agentd binary>`. The script pins Firecracker,
the guest kernel, Chrome for Testing and Deno by hash and Debian by snapshot
date, installing from its `trixie`, `trixie-updates` and `trixie-security`
suites, and rebuilds the root filesystem only when its inputs change. It needs
`mmdebstrap`, `e2fsprogs`, `debian-archive-keyring` and `python3` on the host.
The Debian keyring verifies the pinned snapshot.

Rebuilding the image affects new machines only. Each machine and snapshot keeps
the root image and Firecracker binary it started with, as hard links when the
image dir and state dir share a filesystem and as copies of about 1 GiB when
they do not, so saved memory is always restored against the root filesystem and
Firecracker it was saved with. Replace image files, as the build script does;
never modify them in place.

The state dir holds one directory per machine and per snapshot, a `jails`
directory with one chroot per running VM, and `usage.json`. A directory counts
only once its record file is written, after everything else in it is complete
and synced, and stops counting when the record is removed. A paused machine's
saved state is published the same way, by renaming a complete directory into
place, and is renamed out of place before a VM restored from it may write to the
disk. So nothing half-written is ever restored, and saved memory is never paired
with a disk that a later run changed.

A paused machine uses its memory size on disk, plus what it wrote to its scratch
disk. Saved memory is one file shared by every machine and snapshot that holds
that state: a snapshot of a paused machine adds no second copy, and machines
created from a snapshot share its memory file until each is next paused.

`machined` holds a lock on its state dir.

- On SIGTERM or SIGINT it closes admission, pauses every running machine
  concurrently, and waits until every VM it admitted has exited, including boots
  and restores still in flight, so no VM is left running and the next `machined`
  lists the machines as paused. A machine that cannot be saved is killed and
  comes back exited. Saving writes each machine's memory to disk, so allow for
  that in the service's stop timeout. The saves continue if the caller of
  shutdown is cancelled.
- On start it keeps every recorded snapshot and machine. Paused machines are
  listed as paused; none is resumed automatically. A machine that was running
  when `machined` was killed, or when the host lost power, has lost its memory:
  its Firecracker process, if still alive, is killed, and the machine is listed
  as exited until it is deleted or expires. Every process left in its cgroup is
  killed, and leftover jails removed. Creates, forks, snapshots and saves that
  did not finish are removed, and so are spares.

Both daemons emit JSON logs to stderr, with `RUST_LOG` controlling filtering
(default `info`). Machine lifecycle events (create, pause, resume, fork,
snapshot, destroy, spare boots and the admission limits at startup) include
machine and snapshot IDs, the tenant on create, boot duration and requested
resources; failures include errors. `agentd` process-start events are available
at `debug`.

`GET /metrics` exposes Prometheus text without authentication; it reports the
host, not tenants, so serve it only where the operator can reach it.
`machined_machines_active` counts running VMs, spares and in-flight boots
included, and `machined_memory_reserved_mib` the memory they reserve, while
`machined_memory_budget_mib` reports the admission budget.
`machined_spare_machines` counts spares ready to be taken, and
`machined_machines{state="running"|"paused"|"exited"}` the listed machines.
`machined_creates_total{result="success"|"failure"}` counts validated
`POST /machines` attempts through their terminal outcome, including capacity
rejection, even if the client disconnects. These counters reset at daemon
restart. `agentd` exposes `agentd_processes_running` and
`agentd_processes_retained`; the latter includes finished records kept for
replay.

CI runs the live machine contracts on a Linux KVM runner as root, serially to
bound memory use, and runs the ordinary checks on Linux and macOS.

A machine's own processes run as root inside its guest, and its `agentd` serves
whoever reaches it; `machined` is that machine's only entrance. Egress leaves
from the host's address, as described above. The host itself, its operator and
`machined` are trusted.
