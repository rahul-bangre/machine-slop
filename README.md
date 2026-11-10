# Machine Slop

A computer for agents. `agentd` runs processes, moves files and reaches ports on
a machine over HTTP; `machined` boots Firecracker microVMs that each run
`agentd`. A TypeScript SDK drives both, and benchmarks compare them with hosted
sandboxes.

Requires rustup, Deno (version in `.tool-versions`), and Make.

```sh
make setup
make check
make test
make dev
```

`make dev` starts `agentd` on `127.0.0.1:3000`.

`machined` needs Linux with `/dev/kvm`. On macOS, install
[Lima](https://lima-vm.io) 1.1+ and use the VM, which needs an Apple M3 or later
for nested virtualization:

```sh
make vm-start vm-smoke
make vm-test
make vm-shell
make vm-stop
```

The VM forwards its `agentd` to `127.0.0.1:3001` and `machined` to
`127.0.0.1:4001`. `machined` runs each machine as a jailed Firecracker microVM
owned by a tenant; the VM's API key is in `~/machine-slop/api-key` inside it.

## Documentation

- [apps/agentd](apps/agentd/README.md): files, archives and ports
- [crates/process](crates/process/README.md): process execution
- [crates/machines](crates/machines/README.md): machines and egress
- [bench/agent](bench/agent/README.md): coding agent across sandboxes
- [bench/browser](bench/browser/README.md): browser and observation cost
- [bench/browser-agent](bench/browser-agent/README.md): browser runtimes for a
  model-driven agent
- [bench/env](bench/env/README.md): pinned browser environments for E2B and
  Daytona
