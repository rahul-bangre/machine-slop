# agentd

`agentd` runs processes, moves files and reaches ports on the machine it runs
on. It listens on `127.0.0.1:3000` unless given another address, or
`vsock:<port>` inside a machine. `GET /health` returns `ok`. On vsock it also
relays DNS from `127.0.0.1:53`, over UDP and TCP, to the host's vsock port 1053,
where `machined` answers under the machine's allowlist. Process execution is
described in [crates/process](../../crates/process/README.md).

```ts
import { Machine, tar } from "@machine-slop/sdk";

const machine = new Machine("http://127.0.0.1:3000");
await machine.writeArchive(
  "/workspace/app",
  tar({ "main.py": 'open("out.txt", "w").write("hello")' }),
);
const process = await machine.startProcess({
  program: "python3",
  args: ["main.py"],
  cwd: "/workspace/app",
});
await process.wait();
const output = await machine.readFile("/workspace/app/out.txt");
console.log(new TextDecoder().decode(output));
```

## Files

| Request                | SDK                    | Result                                                       |
| ---------------------- | ---------------------- | ------------------------------------------------------------ |
| `GET /files/{path}`    | `readFile(path)`       | The regular file's bytes, with `Content-Length`              |
| `PUT /files/{path}`    | `writeFile(path, ...)` | Replaces the file with the body; 204                         |
| `DELETE /files/{path}` | `remove(path)`         | Removes the file, or the directory and everything in it; 204 |
| `GET /archives/{path}` | `readArchive(dir)`     | A tar stream of the directory's contents                     |
| `PUT /archives/{path}` | `writeArchive(dir, …)` | Extracts the tar body into the directory; 204                |

Paths are absolute machine paths, one URL-encoded segment per path segment. A
`..` segment returns HTTP 400; the SDK also rejects empty and `.` segments.
Bodies are raw bytes with no size limit beyond the disk.

`PUT /files` creates missing parent directories and writes to a staging file
that is renamed over the target only after the whole body arrives, so readers
see the old file or the new one, never a partial one, and repeating an upload is
safe. `?mode=755` sets permission bits; otherwise a replaced file keeps its bits
and a new file gets `644`. Writes are not synced to disk. A symlink at the path
is written through to its target. The rename gives the path a new inode, so hard
links keep the old contents, and owner and extended attributes are not carried
over.

`DELETE` returns 204 once nothing is at the path, including when nothing was
there. It removes a symlink, not its target.

`GET /archives` writes regular files, directories and symlinks in name order,
with modes and modification times, and skips sockets, pipes and devices. A file
that grows during the walk contributes the length it had when opened; a file
that shrinks, or any other error, aborts the response without an end-of-archive
marker, so the client sees a failed transfer rather than a short archive.

`PUT /archives` creates the directory and merges the archive into it,
overwriting files at the same paths. A malformed archive, or an entry that would
land outside the directory through `..` or a symlink, stops extraction with
HTTP 422. Extraction is not atomic, but repeating the same archive is safe.
`tar(files)` in the SDK builds an archive from a map of relative paths to
contents.

Errors map from the file system: missing path 404, permission 403, wrong kind of
entry 409 (such as reading a directory or a named pipe as a file), full disk
507.

## Ports

```ts
const forward = machine.forward(8000);
console.log(await (await fetch(forward.url)).text());
await forward.close();
```

`GET /ports/{port}` with `Connection: upgrade` and `Upgrade: tcp` connects to
the port on the machine's loopback, `127.0.0.1` then `::1`, and answers 101 once
that connection is open; the HTTP connection then carries raw bytes both ways. A
port with no listener returns 502, and a request without the upgrade
headers 426. `machined` passes the upgrade through, so this works for hosted
machines too.

When either side ends its stream, the tunnel ends. Bytes the port sent before
closing reach the client first. Firecracker's vsock cannot carry a half-close,
so a client that half-closes closes the port's connection; its bytes are
delivered unless the port has not read them within one second.

`machine.connect(port)` returns the tunnel as a readable and writable stream
pair. `machine.forward(port, localPort?)` listens on `127.0.0.1` and opens a
tunnel per accepted connection, so `forward.url` serves the machine's web
server, websockets included. `forward.close()` stops listening and closes open
tunnels.

Non-2xx responses throw `HttpError` with the status. This API assumes a trusted
machine; it provides no authentication, and file access runs with `agentd`'s
privileges. In a machine, `agentd` is reached only through `machined`, which
authenticates the tenant; see [machines](../../crates/machines/README.md).
