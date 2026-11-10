# Machine Slop

A computer for agents.

Requires rustup, Deno (version in `.tool-versions`), and Make.

```sh
make setup
make check
make test
make dev
```

`agentd` listens on `127.0.0.1:3000`; see [apps/agentd](apps/agentd/README.md)
for files, archives and ports.

For Linux development on macOS, install [Lima](https://lima-vm.io) 1.1+. The VM
needs an Apple M3 or later for nested virtualization.

```sh
make vm-start vm-smoke
make vm-test
make vm-shell
make vm-stop
```

In the VM, `agentd` is forwarded to `127.0.0.1:3001` and `machined` to
`127.0.0.1:4001`. `machined` boots each machine as a Firecracker microVM; see
[crates/machines](crates/machines/README.md).
