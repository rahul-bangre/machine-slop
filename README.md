# Machine Slop

A computer for agents.

Requires rustup, Deno (version in `.tool-versions`), and Make.

```sh
make setup
make check
make test
make dev
```

`agentd` listens on `127.0.0.1:3000`.

For Linux development on macOS, install [Lima](https://lima-vm.io) 1.1+:

```sh
make vm-start vm-smoke
make vm-shell
make vm-stop
```
