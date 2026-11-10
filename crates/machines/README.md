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

| Request                 | Result                                                                                     |
| ----------------------- | ------------------------------------------------------------------------------------------ |
| `POST /machines`        | Boots a machine and returns `MachineInfo` once `agentd` answers `/health`                  |
| `GET /machines`         | Lists machines                                                                             |
| `GET /machines/{id}`    | `MachineInfo`                                                                              |
| `PATCH /machines/{id}`  | Replaces the machine's `allowed_hosts` and returns `MachineInfo`                           |
| `DELETE /machines/{id}` | Kills the VM and removes its files                                                         |
| `/machines/{id}/...`    | Forwarded to that machine's `agentd`, upgrades included, so `HostedMachine` is a `Machine` |

A machine defaults to 2 vCPUs and 512 MiB. `boot_ms` runs from the start of the
request until `agentd` is healthy. A create or delete finishes even if its
client disconnects. Creates are not idempotent: after a lost create response,
find the machine with `GET /machines` instead of retrying. A host runs at most
16 machines. A machine whose VM exits on its own stays listed with state
`exited`, and keeps its slot, until it is deleted or expires.

A machine expires after `idle_timeout_secs` (default 600, at most 86400) without
an open request to its `agentd`, and is then destroyed as if deleted. The clock
starts when boot finishes and restarts when the last open request to
`/machines/{id}/...` ends, so a long `wait()`, an event stream or an open port
tunnel keeps the machine alive. Requests to `machined` itself, such as
`GET /machines/{id}`, do not. Processes still running inside an expired machine
are killed with it. To keep an unattended machine, send any request to it, such
as `machine.health()`, within each timeout.

## Machine

The root filesystem is one read-only ext4 image shared by every machine. Writes
go to an overlay on the machine's own sparse 8 GiB scratch disk, so a machine
starts with the image's contents and nothing another machine wrote. `agentd`
runs as root under `tini` in `/workspace` and listens on vsock port 3000;
`machined` reaches it through Firecracker's vsock socket.

## Egress

A machine has no network interface. Its only way out is an HTTPS proxy that
`machined` runs for it. By default it may reach any public host over HTTPS
(`allowed_hosts: ["*"]`). A create request can narrow that list, or turn egress
off with an empty one:

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
normalized. A malformed pattern rejects the request with 422, and so does a name
whose last label is a number, such as `8.0.1`, which the resolver would read as
an address.

`machine.setAllowedHosts(list)` (`PATCH /machines/{id}`) replaces the list while
the machine runs. The new list decides every later connection, and tunnels to
hosts it no longer allows are closed, including ones still connecting, which get
the same 403 as a refused host. Tunnels to hosts it still allows stay open. The
response confirms the stored list; closing affected tunnels follows immediately
but asynchronously, so a few bytes may still pass after it. Tunnels do not count
as activity for the idle timeout: a machine whose only traffic is egress still
expires.

Inside the machine, `HTTPS_PROXY` and `HTTP_PROXY` point at `127.0.0.1:3128`,
where `socat` forwards each connection over vsock to `machined`. `machined`
accepts only `CONNECT host:443` and checks the host against the list. It then
resolves the name itself and connects only to public unicast addresses, so even
`*` cannot reach the host's loopback, private networks or link-local metadata
endpoints. A connection whose local and remote addresses are the same, which is
what reaching a public IP configured on the host looks like, is closed and
refused. Tools that honour the proxy variables work unchanged: curl, git over
HTTPS, pip, npm and Python's `urllib`.

A refusal is an HTTP response to the `CONNECT`, with a plain-text reason: 403
for a port other than 443, a host outside the list, or one with no public
address; 405 for anything but `CONNECT`, plain-HTTP requests included; 400 for a
malformed request; 502 when the name does not resolve or no address accepts
within 3 seconds; and 504 when resolving and connecting take longer than 10
seconds.

Each machine has its own budget, so one machine's traffic cannot refuse
another's: at most 256 open proxy connections, beyond which connections are
closed unanswered, and at most 4 name lookups at a time. At start `machined`
raises its soft open-file limit to hold every machine's full budget (12288 at
the default capacity of 16) and refuses to start if the hard limit is lower. The
proxy and every open tunnel end when the machine's VM exits.

The list limits the destination name a machine asks for and the address
`machined` connects to. It does not inspect the TLS server name or HTTP `Host`
sent inside the tunnel, so another site served from an allowed host's address
may answer there.

Egress leaves from the host's own address, and "public" is an address class, not
a network boundary. Under `*`, the default, a machine reaches anything the host
can reach at a public address, including public-range cloud services that trust
the host, and a host behind 1:1 NAT that reaches its own public IP through the
NAT. Code in such a machine can also send anything the machine holds to any
public HTTPS host, so give machines that hold secrets and run untrusted
instructions a narrower list or `[]`.

Not supported: plain HTTP, ports other than 443, DNS from inside the machine,
UDP, and tools that ignore the proxy variables, such as Node's built-in `fetch`
and git over SSH.

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
