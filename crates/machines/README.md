# Machines

`machined` creates and destroys machines on a Linux host with `/dev/kvm`. Each
machine is a Firecracker microVM running `agentd`.

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

| Request                 | Result                                                                    |
| ----------------------- | ------------------------------------------------------------------------- |
| `POST /machines`        | Boots a machine and returns `MachineInfo` once `agentd` answers `/health` |
| `GET /machines`         | Lists machines                                                            |
| `GET /machines/{id}`    | `MachineInfo`                                                             |
| `DELETE /machines/{id}` | Kills the VM and removes its files                                        |
| `/machines/{id}/...`    | Forwarded to that machine's `agentd`, so `HostedMachine` is a `Machine`   |

A machine defaults to 2 vCPUs and 512 MiB. `boot_ms` runs from the start of the
request until `agentd` is healthy. A create or delete finishes even if its
client disconnects. Creates are not idempotent: after a lost create response,
find the machine with `GET /machines` instead of retrying. A host runs at most
16 machines. A machine whose VM exits on its own stays listed with state
`exited`, and keeps its slot, until it is deleted.

## Machine

The root filesystem is one read-only ext4 image shared by every machine. Writes
go to an overlay on the machine's own sparse 8 GiB scratch disk, so a machine
starts with the image's contents and nothing another machine wrote. `agentd`
runs as root under `tini` in `/workspace` and listens on vsock port 3000;
`machined` reaches it through Firecracker's vsock socket.

The machine has loopback only: no network access.

## Host

`machined <listen address> <image dir> <state dir>`

The image dir holds `firecracker`, `vmlinux` and `rootfs.ext4`, built by
`vm/image/build.sh <image dir> <agentd binary>`. The script pins Firecracker,
the guest kernel and the Debian snapshot by hash, and rebuilds the root
filesystem only when the package set, `agentd` or the init script changes.

`machined` holds a lock on its state dir. On start it kills Firecracker
processes left by an earlier `machined` that died and removes their files. On
SIGTERM or SIGINT it destroys every machine before exiting.

Firecracker runs without its jailer, and the API has no authentication. The host
is trusted.
