# controld

`controld` runs machines across many
[fleet hosts](../../crates/machines/README.md#fleet-hosts) that share one
snapshot store, and owns tenant accounting for the whole fleet. Tenants call it
with the same API and API keys as a standalone `machined`, so the SDK's `Host`
works unchanged, and a snapshot taken on one host starts machines on any host in
its CPU pool.

```sh
SLOP_CONTROL_KEY_FILE=/etc/slop/control.key \
  controld 0.0.0.0:4000 s3://bucket/slop /var/lib/controld /etc/slop/tenants.json
```

The tenants file has `machined`'s format. The control key is a secret shared
with every host: hosts present it to report, `controld` presents it to hosts,
and an operator presents it for `GET /hosts` and `DELETE /hosts?url=`. Run one
`controld` per state dir; it holds a lock on it. Every host must name the store
with the same URL; a report with another one is refused with 409.

| Request                          | Goes to                                                                     |
| -------------------------------- | --------------------------------------------------------------------------- |
| `POST /machines`                 | A reporting host with room, in the snapshot's pool, with the egress address |
| `/machines/{id}` and below       | The machine's host, upgrades included                                       |
| `GET /machines`, `GET /previews` | Every host; 503 if a known host is not reporting                            |
| `POST /snapshots`, `/previews`   | The machine's host                                                          |
| `GET`, `DELETE /snapshots/...`   | The store                                                                   |
| `GET /usage`                     | The tenant's fleet-wide quotas and usage                                    |

## Quotas

A tenant's quotas cover the whole fleet; adding a host never adds to them.
`controld` keeps a ledger in its state dir of every machine, every snapshot and
every request in flight, and checks a request against it before any host hears
of it. A running machine holds its machine count and memory, every machine holds
its provisioned disk, a snapshot holds its `stored_bytes`, and a request in
flight holds what it may take: a create or fork all three, a resume the machine
count and memory, a snapshot the provisional disk. Concurrent requests to
different hosts are checked one at a time against one budget. A request over a
quota returns 429 before anything starts. The create rate is checked the same
way but kept in memory, so a `controld` restart refills it.

A request in flight keeps its hold until the host's answer settles it. While its
answer is unknown, because the host stopped answering, the connection broke or
`controld` restarted, it keeps holding, and a machine whose pause or delete is
in doubt keeps holding as it was. Nothing a host holds is released because the
host is silent: its machines may still run. Holds are released by an answer, by
settling the request, by a host report written after the request was answered
that no longer lists the machine or shows it stopped, or by an operator removing
the host.

## Retries and outcomes in doubt

Every request `controld` sends to change a host carries an operation id, written
to the ledger with the host it goes to before the request is sent. A create that
a host refuses for room, or that cannot reach the host, tries the next host with
room; any other answer is final. A request whose answer is lost returns 502 and
goes nowhere else, except a snapshot, which is settled at once and returns its
result.

`controld` settles such a request on the same host: it asks the host what the
operation made, and the host either answers with the machine or records that the
operation made nothing and will make nothing later. A snapshot is settled in the
store, by writing a record at its id that a late upload cannot replace; a
snapshot that its host answered with a server error is settled this way at once,
since its upload may have finished. A pause, resume or delete is settled by
sending it again, since each is idempotent on the host, and any answer to that
settles it. While a request on a machine is in doubt, other requests on that
machine return 409. Every 5 seconds `controld` settles each request in doubt on
a host that is reporting, except requests it is still waiting on. A host that
has not answered within 65 minutes is treated as having lost the answer.

A create, fork or snapshot with an `Idempotency-Key` returns the same result to
every retry, as on a standalone host: the key belongs to the tenant, is stored
with the request it was first used for, and returns 422 for another request.
Retries with one key run one at a time. A retry of a request in doubt settles it
first and then returns its result, or tries again if it made nothing; if its
host does not answer, the retry returns 503 and holds stay as they are. A key is
free again once its result is deleted. A machine that a create in doubt made and
that was deleted or expired before the create was settled counts as deleted, so
the key is free and a retry makes a new machine, possibly on another host.

If a host reports a machine, or the store holds a snapshot, that the ledger does
not know, `controld` adopts it for its tenant and charges it, even beyond the
tenant's quotas, since it exists.

## Usage

`GET /usage` reports what the ledger has recorded, as of its last write; it is
written at least every 5 seconds. Machine time is metered by the hosts per VM
execution, with a standalone host's boundaries: from when a machine is recorded
or resumed until its VM exits. Reports carry each execution's running total, so
a repeated or reordered report never counts twice, and a host forgets an ended
execution only after `controld` has recorded it. A host that is down reports
nothing meanwhile; its machines' time is counted when it reports again, or, if
it never does, as last reported when an operator removes it.

A snapshot is charged once, for its `stored_bytes`, from when its record was
published to the store until its deletion starts, however many hosts hold
copies. Deleting a snapshot records when deletion started before it touches the
store, so a crash between the two charges no more. A snapshot removed from the
store some other way stops being charged when `controld` next lists the store,
within 30 seconds.

## Hosts

Each host reports every 2 seconds. Every report and every answer from a host
carries the host's start count and a count of changes since, and `controld`
applies only reports newer than anything it has seen from that host, so a
delayed report changes nothing. A host is reporting while a report of its was
applied in the last 10 seconds; only reporting hosts get new machines. A host
that reports a lower start count than it did before has lost its state; its
reports are refused with 409 until an operator removes it. A host joins at its
first report and stays in the ledger, so a restarted `controld` knows hosts that
have not reported yet. `DELETE /hosts?url=` is for a host that is gone for good:
it forgets the host, releases everything its machines held, counts its
executions as last reported, and gives up its requests in doubt. Stop the host
first, since a host that still reports joins again.

Not yet supported: moving a paused machine to another host, so a resume needs
room on its own host; loading memory from the store on demand, so the first
create from a snapshot on a host downloads it first; building snapshots from
Dockerfiles; and running more than one `controld`. Hosts are reached over plain
HTTP.
