#![cfg(target_os = "linux")]

use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use slop_machines::{
    CreateMachine, CreateSnapshot, Limits, MachineError, MachineInfo, MachineState, Machines,
    SnapshotInfo, UpdateMachine,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

const MEMORY_BUDGET_MIB: u32 = 1 << 20;
const DEFAULT_RESERVATION_MIB: u32 = 512 + 64;

struct Host {
    root: PathBuf,
}

impl Host {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("slop-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let image = root.join("image");
        std::fs::create_dir_all(&image).expect("create image dir");
        std::fs::write(image.join("vmlinux"), "kernel").expect("write kernel");
        std::fs::write(image.join("rootfs.ext4"), "rootfs").expect("write rootfs");
        let script = format!(
            "#!/bin/bash\nFAKE_FIRECRACKER='{}' FAKE_FIRECRACKER_ARGS=\"$*\" exec -a firecracker '{}' --exact fake_firecracker --nocapture\n",
            root.display(),
            std::env::current_exe().expect("test binary").display(),
        );
        std::fs::write(image.join("firecracker"), script).expect("write firecracker");
        std::fs::set_permissions(
            image.join("firecracker"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("make firecracker executable");
        Self { root }
    }

    async fn open(&self, capacity: usize) -> Arc<Machines> {
        self.open_with_spares(capacity, 0).await
    }

    async fn open_with_spares(&self, capacity: usize, spares: usize) -> Arc<Machines> {
        let (image, state) = (self.root.join("image"), self.root.join("state"));
        loop {
            let limits = Limits {
                running_vms: capacity,
                memory_mib: MEMORY_BUDGET_MIB,
                spare_machines: spares,
            };
            match Machines::open(image.clone(), state.clone(), limits).await {
                Ok(machines) => return machines,
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(error) => panic!("open machines: {error}"),
            }
        }
    }

    fn fail(&self, request: &str) {
        std::fs::write(self.root.join("fail"), request).expect("write failure");
    }

    fn vm_pauses(&self) -> usize {
        std::fs::read_to_string(self.root.join("requests"))
            .map_or(0, |requests| requests.matches("Paused").count())
    }

    fn disk_len(&self, dir: PathBuf) -> u64 {
        std::fs::metadata(dir.join("scratch.ext4"))
            .expect("disk")
            .len()
    }

    fn vms_started(&self) -> usize {
        std::fs::read_to_string(self.root.join("starts")).map_or(0, |starts| starts.lines().count())
    }

    fn machine_dir(&self, id: &str) -> PathBuf {
        self.root.join("state/machines").join(id)
    }

    fn snapshot_dir(&self, id: &str) -> PathBuf {
        self.root.join("state/snapshots").join(id)
    }

    fn vm_is_alive(&self, id: &str) -> bool {
        self.vms_in(&self.machine_dir(id)) > 0
    }

    fn vms_in(&self, dir: &Path) -> usize {
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
        std::fs::read_dir("/proc")
            .expect("read /proc")
            .filter_map(Result::ok)
            .filter(|process| {
                std::fs::read_link(process.path().join("cwd"))
                    .is_ok_and(|cwd| cwd.starts_with(&dir))
            })
            .count()
    }

    fn all_vms(&self) -> usize {
        self.vms_in(&self.root.join("state/machines"))
    }
}

struct WritableOnDrop(PathBuf);

impl Drop for WritableOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let background_work_settles = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::fs::remove_dir_all(&self.root)
            .is_err_and(|error| error.kind() != std::io::ErrorKind::NotFound)
            && std::time::Instant::now() < background_work_settles
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

async fn create(machines: &Arc<Machines>) -> String {
    let machine = machines
        .create(CreateMachine::default())
        .await
        .expect("create machine");
    assert_eq!(machine.info().state, MachineState::Running);
    machine.id().to_owned()
}

async fn create_from(machines: &Arc<Machines>, snapshot: &SnapshotInfo) -> String {
    let request = CreateMachine {
        snapshot_id: Some(snapshot.id.clone()),
        ..CreateMachine::default()
    };
    let machine = machines.create(request).await.expect("create machine");
    assert_eq!(machine.info().state, MachineState::Running);
    machine.id().to_owned()
}

async fn snapshot(
    machines: &Arc<Machines>,
    machine_id: &str,
) -> Result<SnapshotInfo, MachineError> {
    let request = CreateSnapshot {
        machine_id: machine_id.to_owned(),
    };
    machines.create_snapshot(request).await
}

async fn eventually<F: Future<Output = bool>>(what: &str, done: impl Fn() -> F) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while !done().await {
        assert!(tokio::time::Instant::now() < deadline, "never {what}");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

async fn state(machines: &Machines, id: &str) -> MachineState {
    machines.get(id).await.expect("machine").info().state
}

async fn agent_answers(machines: &Machines, id: &str) -> bool {
    let machine = machines.get(id).await.expect("machine");
    let Ok(mut stream) = machine.connect().await else {
        return false;
    };
    let mut response = String::new();
    stream
        .write_all(b"GET /health HTTP/1.1\r\n\r\n")
        .await
        .is_ok()
        && stream.read_to_string(&mut response).await.is_ok()
        && response.starts_with("HTTP/1.1 200")
}

async fn create_allowing(machines: &Arc<Machines>, hosts: &[&str]) -> MachineInfo {
    let request = CreateMachine {
        allowed_hosts: Some(hosts.iter().map(|host| host.to_string()).collect()),
        ..CreateMachine::default()
    };
    machines
        .create(request)
        .await
        .expect("create machine")
        .info()
}

async fn egress_allows_localhost(machine_dir: &Path) -> bool {
    let mut guest = UnixStream::connect(machine_dir.join("vsock.sock_1080"))
        .await
        .expect("connect to the machine's egress proxy");
    guest
        .write_all(b"CONNECT localhost:443 HTTP/1.1\r\nHost: localhost:443\r\n\r\n")
        .await
        .expect("write");
    let mut refusal = String::new();
    guest.read_to_string(&mut refusal).await.expect("read");
    assert!(refusal.starts_with("HTTP/1.1 403"), "{refusal}");
    refusal.contains("has no public address")
}

fn states(result: Result<MachineInfo, MachineError>) -> MachineState {
    result.expect("transition").state
}

fn is_exited(state: &MachineState) -> bool {
    matches!(state, MachineState::Exited { .. })
}

#[tokio::test]
async fn pause_publishes_a_checkpoint_that_resume_consumes() {
    let host = Host::new("checkpoint");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    let dir = host.machine_dir(&id);

    assert_eq!(states(machines.pause(&id).await), MachineState::Paused);
    assert!(dir.join("checkpoint/vmstate").is_file());
    assert!(dir.join("checkpoint/memory").is_file());
    assert!(!dir.join("checkpoint.tmp").exists());
    assert!(!host.vm_is_alive(&id), "a paused machine has no VM");
    assert!(!agent_answers(&machines, &id).await);
    assert_eq!(states(machines.pause(&id).await), MachineState::Paused);

    assert_eq!(states(machines.resume(&id).await), MachineState::Running);
    assert!(
        !dir.join("checkpoint").exists(),
        "a running machine has no checkpoint to restore a second time"
    );
    assert!(dir.join("restored/memory").is_file());
    assert!(agent_answers(&machines, &id).await);
    assert_eq!(states(machines.resume(&id).await), MachineState::Running);
    assert_eq!(host.vms_started(), 2, "one boot and one restore");

    assert_eq!(states(machines.pause(&id).await), MachineState::Paused);
    assert!(dir.join("checkpoint/memory").is_file());
    assert!(!dir.join("restored").exists());
    machines.destroy(&id).await.expect("destroy");
    assert!(!dir.exists());
}

#[tokio::test]
async fn concurrent_resumes_start_one_vm() {
    let host = Host::new("concurrent");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    machines.pause(&id).await.expect("pause");

    let resumes: Vec<_> = (0..8)
        .map(|_| {
            let (machines, id) = (machines.clone(), id.clone());
            tokio::spawn(async move { machines.resume(&id).await })
        })
        .collect();
    for resume in resumes {
        assert_eq!(states(resume.await.expect("join")), MachineState::Running);
    }
    assert_eq!(host.vms_started(), 2, "one boot and one restore");
    assert!(agent_answers(&machines, &id).await);
}

#[tokio::test]
async fn a_delete_that_overlaps_a_resume_waits_for_it_and_leaves_no_vm() {
    let host = Host::new("delete-race");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    machines.pause(&id).await.expect("pause");

    host.fail("hold-load");
    let resume = {
        let (machines, id) = (machines.clone(), id.clone());
        tokio::spawn(async move { machines.resume(&id).await })
    };
    eventually("started restoring", || async { host.vms_started() == 2 }).await;
    let delete = {
        let (machines, id) = (machines.clone(), id.clone());
        tokio::spawn(async move { machines.destroy(&id).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!delete.is_finished(), "the delete waits for the resume");
    assert!(
        machines.get(&id).await.is_none(),
        "a machine that is being deleted is already unknown"
    );

    host.fail("nothing");
    assert_eq!(states(resume.await.expect("join")), MachineState::Running);
    delete.await.expect("join").expect("destroy");
    assert!(!host.machine_dir(&id).exists());
    assert_eq!(host.all_vms(), 0);
    for late in [machines.resume(&id).await, machines.pause(&id).await] {
        assert!(
            matches!(late, Err(MachineError::UnknownMachine)),
            "{late:?}"
        );
    }
}

#[tokio::test]
async fn a_snapshot_delete_waits_for_a_create_that_is_reading_it() {
    let host = Host::new("snapshot-race");
    let machines = host.open(4).await;
    let source = create(&machines).await;
    let taken = snapshot(&machines, &source).await.expect("snapshot");

    host.fail("hold-load");
    let creating = {
        let (machines, taken) = (machines.clone(), taken.clone());
        tokio::spawn(async move { create_from(&machines, &taken).await })
    };
    eventually("started restoring", || async { host.vms_started() == 2 }).await;
    let delete = {
        let (machines, id) = (machines.clone(), taken.id.clone());
        tokio::spawn(async move { machines.delete_snapshot(&id).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!delete.is_finished(), "the delete waits for the create");

    host.fail("nothing");
    let created = creating.await.expect("join");
    delete.await.expect("join").expect("delete snapshot");
    assert!(!host.snapshot_dir(&taken.id).exists());
    assert!(agent_answers(&machines, &created).await);
}

#[tokio::test]
async fn a_failed_save_leaves_the_machine_running_without_a_checkpoint() {
    let host = Host::new("save-fails");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    let dir = host.machine_dir(&id);

    host.fail("save");
    let error = machines.pause(&id).await.expect_err("pause fails");
    assert!(
        matches!(
            &error,
            MachineError::Failed {
                state: MachineState::Running,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(
        !dir.join("checkpoint").exists() && !dir.join("checkpoint.tmp").exists(),
        "a partly written memory file is not a checkpoint"
    );
    assert!(agent_answers(&machines, &id).await);

    host.fail("nothing");
    assert_eq!(states(machines.pause(&id).await), MachineState::Paused);
}

#[tokio::test]
async fn a_failed_load_leaves_the_machine_paused() {
    let host = Host::new("load-fails");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    machines.pause(&id).await.expect("pause");

    host.fail("load");
    let error = machines.resume(&id).await.expect_err("resume fails");
    assert!(
        matches!(
            &error,
            MachineError::Failed {
                state: MachineState::Paused,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(host.machine_dir(&id).join("checkpoint/memory").is_file());
    assert!(!host.vm_is_alive(&id));

    host.fail("nothing");
    assert_eq!(states(machines.resume(&id).await), MachineState::Running);
}

#[tokio::test]
async fn a_resume_that_fails_once_the_checkpoint_is_retired_loses_the_machine() {
    let host = Host::new("resume-fails");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    machines.pause(&id).await.expect("pause");

    host.fail("resume");
    let error = machines.resume(&id).await.expect_err("resume fails");
    assert!(
        matches!(&error, MachineError::Failed { state, .. } if is_exited(state)),
        "{error:?}"
    );
    assert!(
        !host.machine_dir(&id).join("checkpoint").exists(),
        "the guest may have written to the disk the checkpoint described"
    );
    assert!(!host.vm_is_alive(&id));
    host.fail("nothing");
    for retry in [machines.resume(&id).await, machines.pause(&id).await] {
        assert!(matches!(retry, Err(MachineError::Conflict(_))), "{retry:?}");
    }

    drop(machines);
    let machines = host.open(4).await;
    assert!(is_exited(&state(&machines, &id).await));
}

#[tokio::test]
async fn a_restart_keeps_what_was_acknowledged_and_nothing_else() {
    let host = Host::new("restart");
    let machines = host.open(8).await;
    let paused = create(&machines).await;
    machines.pause(&paused).await.expect("pause");
    let kept = snapshot(&machines, &paused).await.expect("snapshot");
    let resumed = create(&machines).await;
    machines.pause(&resumed).await.expect("pause");
    machines.resume(&resumed).await.expect("resume");
    let running = create(&machines).await;

    let unrecorded = host.machine_dir("unrecorded");
    std::fs::create_dir(&unrecorded).expect("create dir");
    std::fs::write(unrecorded.join("scratch.ext4"), "partial").expect("write");
    let staging = host.machine_dir(&running).join("checkpoint.tmp");
    std::fs::create_dir(&staging).expect("create dir");
    std::fs::write(staging.join("memory"), "partial").expect("write");
    let partial_snapshot = host.snapshot_dir("partial");
    std::fs::create_dir(&partial_snapshot).expect("create dir");
    std::fs::write(partial_snapshot.join("memory"), "partial").expect("write");

    drop(machines);
    assert!(host.vm_is_alive(&running), "machined died without cleanup");
    let machines = host.open(8).await;

    assert_eq!(state(&machines, &paused).await, MachineState::Paused);
    for lost in [&running, &resumed] {
        assert!(
            is_exited(&state(&machines, lost).await),
            "memory that was never saved, or was saved before a later run, is not restored"
        );
        assert!(!host.vm_is_alive(lost));
        assert!(matches!(
            machines.resume(lost).await,
            Err(MachineError::Conflict(_))
        ));
    }
    assert_eq!(machines.list().await.len(), 3);
    assert!(!unrecorded.exists() && !staging.exists() && !partial_snapshot.exists());
    let snapshots = machines.snapshots().await;
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].id, kept.id);

    assert_eq!(
        states(machines.resume(&paused).await),
        MachineState::Running
    );
    create_from(&machines, &kept).await;
}

#[tokio::test]
async fn a_snapshot_outlives_its_source_and_its_deletion_spares_its_machines() {
    let host = Host::new("snapshot");
    let machines = host.open(4).await;
    let source = create(&machines).await;
    machines.pause(&source).await.expect("pause");
    let taken = snapshot(&machines, &source).await.expect("snapshot");
    assert_eq!(taken.machine_id, source);
    let snapshot_dir = host.snapshot_dir(&taken.id);

    let inode = |path: PathBuf| std::fs::metadata(path).expect("metadata").ino();
    assert_ne!(
        inode(snapshot_dir.join("scratch.ext4")),
        inode(host.machine_dir(&source).join("scratch.ext4")),
        "a snapshot's disk is its own"
    );
    machines.resume(&source).await.expect("resume");
    machines.destroy(&source).await.expect("destroy");

    let first = create_from(&machines, &taken).await;
    let second = create_from(&machines, &taken).await;
    let disks = [
        inode(snapshot_dir.join("scratch.ext4")),
        inode(host.machine_dir(&first).join("scratch.ext4")),
        inode(host.machine_dir(&second).join("scratch.ext4")),
    ];
    assert!(disks[0] != disks[1] && disks[0] != disks[2] && disks[1] != disks[2]);

    machines.delete_snapshot(&taken.id).await.expect("delete");
    assert!(!snapshot_dir.exists());
    assert!(machines.snapshots().await.is_empty());
    let request = CreateMachine {
        snapshot_id: Some(taken.id.clone()),
        ..CreateMachine::default()
    };
    assert!(matches!(
        machines.create(request).await,
        Err(MachineError::UnknownSnapshot)
    ));
    assert!(matches!(
        machines.delete_snapshot(&taken.id).await,
        Err(MachineError::UnknownSnapshot)
    ));

    for machine in [&first, &second] {
        assert!(agent_answers(&machines, machine).await);
        assert_eq!(states(machines.pause(machine).await), MachineState::Paused);
        assert_eq!(
            states(machines.resume(machine).await),
            MachineState::Running
        );
    }
}

#[tokio::test]
async fn capacity_counts_running_vms_and_not_paused_machines() {
    let host = Host::new("capacity");
    let machines = host.open(1).await;
    let parked = create(&machines).await;
    let full = machines.create(CreateMachine::default()).await;
    assert!(matches!(full, Err(MachineError::Capacity)));
    let full = machines.fork(&parked).await;
    assert!(matches!(full, Err(MachineError::Capacity)));
    assert_eq!(host.vm_pauses(), 0, "a refused fork never stops its source");
    assert!(agent_answers(&machines, &parked).await);

    machines.pause(&parked).await.expect("pause");
    let running = create(&machines).await;
    let full = machines.resume(&parked).await;
    assert!(matches!(full, Err(MachineError::Capacity)), "{full:?}");
    assert_eq!(state(&machines, &parked).await, MachineState::Paused);
    assert!(
        host.machine_dir(&parked)
            .join("checkpoint/memory")
            .is_file()
    );

    machines.destroy(&running).await.expect("destroy");
    assert_eq!(
        states(machines.resume(&parked).await),
        MachineState::Running
    );
    assert_eq!(host.all_vms(), 1);
}

#[tokio::test]
async fn a_fork_or_snapshot_of_a_running_machine_leaves_it_running() {
    let host = Host::new("fork");
    let machines = host.open(4).await;
    let source = create(&machines).await;

    let fork = machines.fork(&source).await.expect("fork");
    assert_eq!(fork.info().state, MachineState::Running);
    assert_ne!(fork.id(), source);
    assert_eq!(
        host.disk_len(host.machine_dir(fork.id())),
        host.disk_len(host.machine_dir(&source)),
        "the fork got the disk as it was while its source was stopped"
    );
    let taken = snapshot(&machines, &source).await.expect("snapshot");
    assert_eq!(
        host.disk_len(host.snapshot_dir(&taken.id)) + 1,
        host.disk_len(host.machine_dir(&source)),
        "the snapshot's disk was copied before its source ran again"
    );
    let from_snapshot = create_from(&machines, &taken).await;
    for machine in [&source, fork.id(), &from_snapshot] {
        assert!(agent_answers(&machines, machine).await);
    }
    assert!(
        !host.machine_dir(&source).join("checkpoint").exists(),
        "a machine that keeps running has no checkpoint"
    );
    let inode = |dir: PathBuf| {
        std::fs::metadata(dir.join("scratch.ext4"))
            .expect("disk")
            .ino()
    };
    assert_ne!(
        inode(host.machine_dir(&source)),
        inode(host.machine_dir(fork.id()))
    );
    assert_eq!(
        std::fs::read_dir(host.root.join("state/snapshots"))
            .expect("snapshots")
            .count(),
        1,
        "a fork leaves no saved state behind"
    );

    machines.pause(&source).await.expect("pause");
    let of_paused = machines.fork(&source).await.expect("fork");
    assert!(agent_answers(&machines, of_paused.id()).await);
    assert_eq!(state(&machines, &source).await, MachineState::Paused);
    assert_eq!(
        states(machines.resume(&source).await),
        MachineState::Running
    );
}

#[tokio::test]
async fn a_failed_fork_or_snapshot_leaves_the_machine_running_and_nothing_else() {
    let host = Host::new("fork-fails");
    let machines = host.open(4).await;
    let source = create(&machines).await;

    host.fail("save");
    let fork = machines.fork(&source).await.map(|fork| fork.info());
    let taken = snapshot(&machines, &source).await;
    for error in [
        fork.expect_err("fork fails"),
        taken.expect_err("snapshot fails"),
    ] {
        assert!(
            matches!(
                &error,
                MachineError::Failed {
                    state: MachineState::Running,
                    ..
                }
            ),
            "{error:?}"
        );
    }
    assert!(agent_answers(&machines, &source).await);
    assert_eq!(machines.list().await.len(), 1);
    assert!(machines.snapshots().await.is_empty());
    let leftovers = |dir: &str| std::fs::read_dir(host.root.join(dir)).expect("dir").count();
    assert_eq!(
        (leftovers("state/machines"), leftovers("state/snapshots")),
        (1, 0)
    );

    host.fail("load");
    let fork = machines.fork(&source).await.map(|fork| fork.info());
    assert!(matches!(fork, Err(MachineError::Boot(_))), "{fork:?}");
    assert!(agent_answers(&machines, &source).await);
    assert_eq!(
        (leftovers("state/machines"), leftovers("state/snapshots")),
        (1, 0)
    );
    assert_eq!(host.all_vms(), 1);
}

#[tokio::test]
async fn a_create_takes_a_spare_that_is_replaced_and_never_recorded() {
    let host = Host::new("spares");
    let machines = host.open_with_spares(2, 1).await;
    eventually("booted a spare", || async { machines.spares().await == 1 }).await;
    assert_eq!(host.vms_started(), 1);
    assert!(machines.list().await.is_empty(), "a spare is not a machine");

    let first = create(&machines).await;
    assert!(agent_answers(&machines, &first).await);
    eventually("replaced the spare", || async {
        machines.spares().await == 1
    })
    .await;
    assert_eq!(host.vms_started(), 2, "the create booted nothing itself");

    let second = create(&machines).await;
    assert_eq!(host.vms_started(), 2);
    let full = machines.create(CreateMachine::default()).await;
    assert!(matches!(full, Err(MachineError::Capacity)));
    assert_eq!(machines.spares().await, 0);

    machines.destroy(&second).await.expect("destroy");
    eventually("used the freed slot", || async {
        machines.spares().await == 1
    })
    .await;
    let sized = CreateMachine {
        vcpus: Some(1),
        ..CreateMachine::default()
    };
    let sized = machines.create(sized).await.expect("create");
    assert_eq!(
        (machines.spares().await, host.all_vms()),
        (0, 2),
        "a spare gives its slot up to a machine it cannot become"
    );
    machines.destroy(sized.id()).await.expect("destroy");
    eventually("used the freed slot", || async {
        machines.spares().await == 1
    })
    .await;

    drop((machines, full, sized));
    assert_eq!(host.all_vms(), 2, "machined died without cleanup");
    let machines = host.open(2).await;
    assert_eq!(host.all_vms(), 0);
    assert_eq!(machines.list().await.len(), 1);
    assert!(is_exited(&state(&machines, &first).await));
    let dirs = std::fs::read_dir(host.root.join("state/machines")).expect("machines");
    assert_eq!(dirs.count(), 1, "the spare was never a machine");
}

#[tokio::test]
async fn a_spare_that_is_still_booting_yields_its_slot_too() {
    let host = Host::new("spare-yields");
    let machines = host.open_with_spares(2, 1).await;
    eventually("booted a spare", || async { machines.spares().await == 1 }).await;

    let first = create(&machines).await;
    let second = create(&machines).await;
    assert_eq!((host.vms_started(), host.all_vms()), (2, 2));

    machines.pause(&first).await.expect("pause");
    machines.pause(&second).await.expect("pause");
    assert_eq!(states(machines.resume(&first).await), MachineState::Running);
    assert_eq!(
        states(machines.resume(&second).await),
        MachineState::Running
    );
    let full = machines.create(CreateMachine::default()).await;
    assert!(matches!(full, Err(MachineError::Capacity)));
    assert_eq!((machines.spares().await, host.all_vms()), (0, 2));
}

#[tokio::test]
async fn a_slot_freed_for_a_request_is_not_taken_by_a_new_spare() {
    let host = Host::new("spare-steal");
    let machines = host.open_with_spares(2, 1).await;
    let parked = create(&machines).await;
    let idle = create(&machines).await;
    machines.pause(&parked).await.expect("pause");
    machines.pause(&idle).await.expect("pause");
    let running = create(&machines).await;
    eventually("booted a spare", || async { machines.spares().await == 1 }).await;

    let noise = {
        let (machines, idle) = (machines.clone(), idle.clone());
        tokio::spawn(async move {
            loop {
                machines.pause(&idle).await.expect("pause");
            }
        })
    };
    let started = host.vms_started();
    let resume = machines.resume(&parked);
    let resumed = tokio::time::timeout(std::time::Duration::from_secs(20), resume)
        .await
        .expect("the resume is not starved of its slot");
    assert_eq!(states(resumed), MachineState::Running);
    assert_eq!(
        host.vms_started(),
        started + 1,
        "the resume evicted the spare, restored one VM, and waited for no other"
    );
    noise.abort();

    machines.destroy(&running).await.expect("destroy");
    eventually("booted a spare again", || async {
        machines.spares().await == 1
    })
    .await;
    let exited = create(&machines).await;
    host.fail("resume");
    machines.pause(&exited).await.expect("pause");
    machines.resume(&exited).await.expect_err("resume fails");
    host.fail("nothing");
    eventually("booted a spare again", || async {
        machines.spares().await == 1
    })
    .await;
    let fork = machines.fork(&exited).await.map(|fork| fork.info());
    assert!(matches!(fork, Err(MachineError::Conflict(_))), "{fork:?}");
    assert_eq!(
        machines.spares().await,
        1,
        "a fork that cannot happen takes no slot from a spare"
    );
}

#[tokio::test]
async fn a_checkpoint_is_never_in_place_while_it_is_being_written() {
    let host = Host::new("checkpoint-staged");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    let dir = host.machine_dir(&id);

    host.fail("hold-save");
    let pausing = {
        let (machines, id) = (machines.clone(), id.clone());
        tokio::spawn(async move { machines.pause(&id).await })
    };
    let staged = dir.join("checkpoint.tmp/memory");
    eventually("started saving", || async { staged.is_file() }).await;
    assert!(
        !dir.join("checkpoint").exists(),
        "memory that is still being written is not a checkpoint"
    );

    host.fail("nothing");
    assert_eq!(states(pausing.await.expect("join")), MachineState::Paused);
    let memory = std::fs::read_to_string(dir.join("checkpoint/memory")).expect("memory");
    assert_eq!(memory, "memory");
    assert!(!dir.join("checkpoint.tmp").exists());
}

#[tokio::test]
async fn a_resume_that_cannot_retire_the_checkpoint_leaves_the_machine_paused() {
    let host = Host::new("retire-fails");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    machines.pause(&id).await.expect("pause");
    let dir = host.machine_dir(&id);

    host.fail("block-retire");
    let error = machines.resume(&id).await.expect_err("resume fails");
    assert!(
        matches!(
            &error,
            MachineError::Failed {
                state: MachineState::Paused,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(dir.join("checkpoint/memory").is_file());
    assert!(!host.vm_is_alive(&id), "the guest never ran on that disk");
    assert_eq!(host.vm_pauses(), 1, "only the original pause");

    host.fail("nothing");
    std::fs::remove_file(dir.join("restored")).expect("unblock retire");
    assert_eq!(states(machines.resume(&id).await), MachineState::Running);
}

#[tokio::test]
async fn an_expired_machine_that_cannot_be_deleted_stays_listed_until_it_can() {
    if rustix::process::getuid().is_root() {
        return;
    }
    let host = Host::new("expiry-fails");
    let machines = host.open(4).await;
    let request = CreateMachine {
        idle_timeout_secs: Some(1),
        ..CreateMachine::default()
    };
    let id = machines.create(request).await.expect("create").info().id;
    let dir = host.machine_dir(&id);
    let set_mode = |mode| {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
    };
    let _writable_again_for_cleanup = WritableOnDrop(dir.clone());

    set_mode(0o555);
    eventually("listed again, exited, after a failed expiry", || async {
        let listed = machines.get(&id).await.map(|machine| machine.info().state);
        listed.as_ref().is_some_and(is_exited)
    })
    .await;
    let half_the_idle_timeout = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    while tokio::time::Instant::now() < half_the_idle_timeout {
        assert!(
            machines.get(&id).await.is_some(),
            "a failed expiry waits a full idle timeout before trying again"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    set_mode(0o755);
    assert!(dir.join("machine.json").is_file());
    assert!(!host.vm_is_alive(&id));

    eventually("expired once its record could go", || async {
        machines.get(&id).await.is_none() && !dir.exists()
    })
    .await;
}

#[tokio::test]
async fn a_delete_that_cannot_remove_the_record_leaves_the_machine_as_it_was() {
    if rustix::process::getuid().is_root() {
        return;
    }
    let host = Host::new("delete-fails");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    machines.pause(&id).await.expect("pause");
    let dir = host.machine_dir(&id);
    let set_mode = |mode| {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
    };
    let _writable_again_for_cleanup = WritableOnDrop(dir.clone());

    set_mode(0o555);
    let failed = machines.destroy(&id).await;
    set_mode(0o755);
    assert!(matches!(failed, Err(MachineError::Io(_))), "{failed:?}");
    assert_eq!(state(&machines, &id).await, MachineState::Paused);

    drop(machines);
    let machines = host.open(4).await;
    assert_eq!(states(machines.resume(&id).await), MachineState::Running);
    machines.destroy(&id).await.expect("destroy");
    assert!(!dir.exists());
}

#[tokio::test]
async fn allowed_hosts_survive_pause_and_restart_and_pass_to_forks_and_snapshots() {
    let host = Host::new("egress");
    let machines = host.open(8).await;
    let source = create_allowing(&machines, &["localhost"]).await.id;
    assert!(egress_allows_localhost(&host.machine_dir(&source)).await);

    machines.pause(&source).await.expect("pause");
    let narrowed = UpdateMachine {
        allowed_hosts: vec!["example.com".into()],
    };
    let updated = machines.update(&source, narrowed).await.expect("update");
    assert_eq!(updated.allowed_hosts, ["example.com"]);
    drop(machines);
    let machines = host.open(8).await;
    assert_eq!(
        states(machines.resume(&source).await),
        MachineState::Running
    );
    let resumed = machines.get(&source).await.expect("machine").info();
    assert_eq!(resumed.allowed_hosts, ["example.com"]);
    assert!(!egress_allows_localhost(&host.machine_dir(&source)).await);

    let fork = machines.fork(&source).await.expect("fork").info();
    assert_eq!(fork.allowed_hosts, ["example.com"]);
    assert_eq!(fork.idle_timeout_secs, resumed.idle_timeout_secs);
    assert!(!egress_allows_localhost(&host.machine_dir(&fork.id)).await);

    machines.pause(&source).await.expect("pause");
    host.fail("hold-load");
    let started = host.vms_started();
    let resuming = {
        let (machines, source) = (machines.clone(), source.clone());
        tokio::spawn(async move { machines.resume(&source).await })
    };
    eventually("started restoring", || async {
        host.vms_started() > started
    })
    .await;
    let revoking = {
        let (machines, source) = (machines.clone(), source.clone());
        let none = UpdateMachine {
            allowed_hosts: Vec::new(),
        };
        tokio::spawn(async move { machines.update(&source, none).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let (forking, snapshotting) = {
        let (forks, snapshots, source) = (machines.clone(), machines.clone(), source.clone());
        let fork_source = source.clone();
        (
            tokio::spawn(async move { forks.fork(&fork_source).await.map(|fork| fork.info()) }),
            tokio::spawn(async move { snapshot(&snapshots, &source).await }),
        )
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    host.fail("nothing");
    resuming.await.expect("join").expect("resume");
    revoking.await.expect("join").expect("update");
    let after_revocation = forking.await.expect("join").expect("fork");
    assert!(
        after_revocation.allowed_hosts.is_empty(),
        "a fork that waited behind a revocation does not keep what was revoked"
    );
    let after_revocation = snapshotting.await.expect("join").expect("snapshot");
    assert!(after_revocation.allowed_hosts.is_empty());
    let restored = UpdateMachine {
        allowed_hosts: vec!["example.com".into()],
    };
    machines.update(&source, restored).await.expect("update");

    let taken = snapshot(&machines, &source).await.expect("snapshot");
    assert_eq!(taken.allowed_hosts, ["example.com"]);
    let inheriting = create_from(&machines, &taken).await;
    assert!(!egress_allows_localhost(&host.machine_dir(&inheriting)).await);
    let overriding = CreateMachine {
        snapshot_id: Some(taken.id.clone()),
        allowed_hosts: Some(vec!["localhost".into()]),
        ..CreateMachine::default()
    };
    let overriding = machines.create(overriding).await.expect("create").info();
    assert_eq!(overriding.allowed_hosts, ["localhost"]);
    assert!(egress_allows_localhost(&host.machine_dir(&overriding.id)).await);
}

#[tokio::test]
async fn a_spare_has_no_egress_until_a_create_gives_it_that_machines_list() {
    let host = Host::new("spare-egress");
    let machines = host.open_with_spares(2, 1).await;
    eventually("booted a spare", || async { machines.spares().await == 1 }).await;
    let spare_dir = std::fs::read_dir(host.root.join("state/machines"))
        .expect("machines")
        .next()
        .expect("the spare's directory")
        .expect("entry")
        .path();
    assert!(!egress_allows_localhost(&spare_dir).await);

    let taken = create_allowing(&machines, &["localhost"]).await;
    assert_eq!(host.machine_dir(&taken.id), spare_dir);
    assert_eq!(taken.allowed_hosts, ["localhost"]);
    assert!(egress_allows_localhost(&spare_dir).await);
}

#[tokio::test]
async fn a_paused_machine_does_not_expire_and_a_resumed_one_does() {
    let host = Host::new("idle");
    let machines = host.open(4).await;
    let request = CreateMachine {
        idle_timeout_secs: Some(1),
        ..CreateMachine::default()
    };
    let id = machines.create(request).await.expect("create").info().id;
    machines.pause(&id).await.expect("pause");
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert_eq!(state(&machines, &id).await, MachineState::Paused);

    drop(machines);
    let machines = host.open(4).await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(state(&machines, &id).await, MachineState::Paused);

    assert_eq!(states(machines.resume(&id).await), MachineState::Running);
    eventually("expired", || async { machines.get(&id).await.is_none() }).await;
    eventually("removed", || async { !host.machine_dir(&id).exists() }).await;
    assert_eq!(host.all_vms(), 0);

    let request = CreateMachine {
        idle_timeout_secs: Some(1),
        ..CreateMachine::default()
    };
    let lost = machines.create(request).await.expect("create").info().id;
    machines.pause(&lost).await.expect("pause");
    host.fail("resume");
    machines.resume(&lost).await.expect_err("resume fails");
    assert!(is_exited(&state(&machines, &lost).await));
    eventually("expired once it was no longer paused", || async {
        machines.get(&lost).await.is_none()
    })
    .await;
}

#[tokio::test]
async fn shutdown_pauses_running_machines_and_refuses_to_start_more() {
    let host = Host::new("shutdown");
    let machines = host.open_with_spares(4, 1).await;
    let running = create(&machines).await;
    let paused = create(&machines).await;
    machines.pause(&paused).await.expect("pause");
    eventually("booted a spare", || async { machines.spares().await == 1 }).await;
    let taken = snapshot(&machines, &paused).await.expect("snapshot");
    host.fail("hold-load");
    let started = host.vms_started();
    let overtaken = {
        let request = CreateMachine {
            snapshot_id: Some(taken.id.clone()),
            ..CreateMachine::default()
        };
        let machines = machines.clone();
        tokio::spawn(async move { machines.create(request).await.map(|machine| machine.info()) })
    };
    eventually("started restoring", || async {
        host.vms_started() > started
    })
    .await;

    let shutting_down = {
        let machines = machines.clone();
        tokio::spawn(async move { machines.shutdown().await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !shutting_down.is_finished(),
        "shutdown waits for a VM that was admitted before it began"
    );
    host.fail("nothing");
    tokio::time::timeout(std::time::Duration::from_secs(20), shutting_down)
        .await
        .expect("shutdown finishes once the admitted restore does")
        .expect("join");
    let overtaken = overtaken.await.expect("join");
    assert!(
        matches!(overtaken, Err(MachineError::ShuttingDown)),
        "{overtaken:?}"
    );
    assert_eq!(
        host.all_vms(),
        0,
        "no VM outlives a shutdown: not a spare, nor a create it overtook"
    );
    assert_eq!(machines.list().await.len(), 2);
    assert_eq!(
        machines.resource_usage(),
        (0, 0, MEMORY_BUDGET_MIB),
        "every reservation is back once shutdown returns"
    );
    for id in [&running, &paused] {
        assert_eq!(state(&machines, id).await, MachineState::Paused);
        assert!(!host.vm_is_alive(id));
        assert!(matches!(
            machines.resume(id).await,
            Err(MachineError::ShuttingDown)
        ));
    }
    assert!(matches!(
        machines.create(CreateMachine::default()).await,
        Err(MachineError::ShuttingDown)
    ));
    assert!(matches!(
        machines.fork(&paused).await,
        Err(MachineError::ShuttingDown)
    ));

    drop(machines);
    let machines = host.open(4).await;
    for id in [&running, &paused] {
        assert_eq!(states(machines.resume(id).await), MachineState::Running);
    }
}

#[tokio::test]
async fn shutdown_saves_machines_concurrently_and_keeps_going_when_cancelled() {
    let host = Host::new("shutdown-cancelled");
    let machines = host.open(4).await;
    let ids = [
        create(&machines).await,
        create(&machines).await,
        create(&machines).await,
    ];

    host.fail("hold-save");
    let shutting_down = {
        let machines = machines.clone();
        tokio::spawn(async move { machines.shutdown().await })
    };
    eventually("every machine is being saved at once", || async {
        ids.iter()
            .all(|id| host.machine_dir(id).join("checkpoint.tmp/memory").is_file())
    })
    .await;
    shutting_down.abort();
    assert!(shutting_down.await.expect_err("aborted").is_cancelled());

    host.fail("nothing");
    eventually("saved after the shutdown call was cancelled", || async {
        host.all_vms() == 0
    })
    .await;
    for id in &ids {
        assert_eq!(state(&machines, id).await, MachineState::Paused);
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), machines.shutdown())
        .await
        .expect("a second shutdown finds nothing left to do");
    assert_eq!(machines.resource_usage(), (0, 0, MEMORY_BUDGET_MIB));
}

#[tokio::test]
async fn memory_is_reserved_by_running_vms_and_given_up_by_spares() {
    let host = Host::new("memory");
    let budget = 2 * DEFAULT_RESERVATION_MIB;
    let (image, state) = (host.root.join("image"), host.root.join("state"));
    let limits = Limits {
        running_vms: 8,
        memory_mib: budget,
        spare_machines: 1,
    };
    let machines = Machines::open(image, state, limits).await.expect("open");
    eventually("booted a spare", || async { machines.spares().await == 1 }).await;
    assert_eq!(
        machines.resource_usage(),
        (1, DEFAULT_RESERVATION_MIB, budget)
    );

    let sized = CreateMachine {
        memory_mib: Some(2048),
        ..CreateMachine::default()
    };
    let refused = machines.create(sized).await.map(|machine| machine.info());
    assert!(
        matches!(refused, Err(MachineError::Capacity)),
        "a machine larger than the whole budget is refused: {refused:?}"
    );
    assert_eq!(
        machines.spares().await,
        1,
        "a request that can never fit costs no spare"
    );
    let first = create(&machines).await;
    let second = machines
        .create(CreateMachine {
            vcpus: Some(1),
            ..CreateMachine::default()
        })
        .await
        .expect("a spare gives its memory up to a machine it cannot become")
        .info()
        .id;
    assert_eq!(machines.spares().await, 0);
    assert_eq!(machines.resource_usage(), (2, budget, budget));
    let full = machines.create(CreateMachine::default()).await;
    assert!(matches!(full, Err(MachineError::Capacity)));
    assert_eq!(
        std::fs::read_dir(host.root.join("state/machines"))
            .expect("machines")
            .count(),
        2,
        "a refused create leaves no files"
    );

    machines.pause(&first).await.expect("pause");
    eventually(
        "a spare took the memory the paused machine gave up",
        || async { machines.spares().await == 1 },
    )
    .await;
    assert_eq!(
        machines.resource_usage(),
        (2, budget, budget),
        "only the running machine and the spare reserve memory"
    );
    machines.destroy(&second).await.expect("destroy");
    assert_eq!(states(machines.resume(&first).await), MachineState::Running);
    machines.destroy(&first).await.expect("destroy");
    eventually("booted a spare again", || async {
        machines.spares().await == 1
    })
    .await;
    assert_eq!(
        machines.resource_usage(),
        (1, DEFAULT_RESERVATION_MIB, budget)
    );
    let (successes, failures) = machines.create_counts();
    assert_eq!((successes, failures), (2, 2));
}

#[tokio::test]
async fn requests_that_name_nothing_or_contradict_a_snapshot_are_rejected() {
    let host = Host::new("invalid");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    machines.pause(&id).await.expect("pause");
    let taken = snapshot(&machines, &id).await.expect("snapshot");

    let resized = CreateMachine {
        snapshot_id: Some(taken.id),
        vcpus: Some(1),
        ..CreateMachine::default()
    };
    assert!(matches!(
        machines.create(resized).await,
        Err(MachineError::Invalid(_))
    ));
    let missing = CreateMachine {
        snapshot_id: Some("missing".into()),
        ..CreateMachine::default()
    };
    assert!(matches!(
        machines.create(missing).await,
        Err(MachineError::UnknownSnapshot)
    ));
    assert!(matches!(
        machines.pause("missing").await,
        Err(MachineError::UnknownMachine)
    ));
    assert!(matches!(
        machines.resume("missing").await,
        Err(MachineError::UnknownMachine)
    ));
    assert!(matches!(
        snapshot(&machines, "missing").await,
        Err(MachineError::UnknownMachine)
    ));
    assert!(matches!(
        machines.fork("missing").await,
        Err(MachineError::UnknownMachine)
    ));
    assert!(matches!(
        machines.destroy("missing").await,
        Err(MachineError::UnknownMachine)
    ));
    assert_eq!(host.vms_started(), 1);
}

#[tokio::test]
async fn fake_firecracker() {
    let Ok(root) = std::env::var("FAKE_FIRECRACKER") else {
        return;
    };
    let root = PathBuf::from(root);
    let booted =
        std::env::var("FAKE_FIRECRACKER_ARGS").is_ok_and(|args| args.contains("--config-file"));
    append(&root.join("starts"), "start\n");

    let vm = Arc::new(std::sync::Mutex::new(FakeVm {
        configured: booted,
        running: booted,
    }));
    let api = UnixListener::bind("api.sock").expect("bind api socket");
    let vsock = UnixListener::bind("vsock.sock").expect("bind vsock socket");
    let agent = vm.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = vsock.accept().await.expect("accept");
            let running = agent.lock().expect("vm").running;
            tokio::spawn(answer_health(stream, running));
        }
    });
    loop {
        let (stream, _) = api.accept().await.expect("accept");
        answer_api(stream, &vm, &root.join("fail")).await;
    }
}

fn append(path: &Path, text: &str) {
    let mut file = std::fs::File::options()
        .create(true)
        .append(true)
        .open(path)
        .expect("open for append");
    std::io::Write::write_all(&mut file, text.as_bytes()).expect("append");
}

struct FakeVm {
    configured: bool,
    running: bool,
}

async fn answer_health(stream: UnixStream, running: bool) {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    if stream.read_line(&mut line).await.is_err() || !line.starts_with("CONNECT ") {
        return;
    }
    let _ = stream.write_all(b"OK 1\n").await;
    while line != "\r\n" {
        line.clear();
        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
    }
    if running {
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await;
    }
}

async fn answer_api(stream: UnixStream, vm: &std::sync::Mutex<FakeVm>, fail: &Path) {
    let mut stream = BufReader::new(stream);
    let mut request_line = String::new();
    stream.read_line(&mut request_line).await.expect("request");
    let mut length = 0;
    loop {
        let mut header = String::new();
        stream.read_line(&mut header).await.expect("header");
        match header.trim_end().split_once(": ") {
            Some(("Content-Length", value)) => length = value.parse().expect("length"),
            Some(_) => {}
            None => break,
        }
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.expect("body");
    let body = String::from_utf8(body).expect("utf-8 body");
    let field = |name: &str| {
        let (_, rest) = body.split_once(&format!("\"{name}\":\""))?;
        Some(rest.split_once('"')?.0.to_owned())
    };

    let request = request_line.trim_end().trim_end_matches(" HTTP/1.1");
    append(
        &fail.with_file_name("requests"),
        &format!("{request} {body}\n"),
    );
    let held = match request {
        "PUT /snapshot/load" => "hold-load",
        "PUT /snapshot/create" => "hold-save",
        _ => "",
    };
    let failing = loop {
        let failing = std::fs::read_to_string(fail).unwrap_or_default();
        if held.is_empty() || failing != held {
            break failing;
        }
        if held == "hold-save" {
            std::fs::write(field("mem_file_path").expect("memory path"), "partial")
                .expect("write memory");
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    let outcome = {
        let mut vm = vm.lock().expect("vm");
        match request {
            "PATCH /vm" if body.contains("Paused") => {
                vm.running = false;
                Ok(())
            }
            "PATCH /vm" if failing == "resume" || !vm.configured => Err("cannot resume"),
            "PATCH /vm" if Path::new("checkpoint").exists() => Err("checkpoint is published"),
            "PATCH /vm" => {
                vm.running = true;
                append(Path::new("scratch.ext4"), "w");
                Ok(())
            }
            "PUT /snapshot/create" if vm.running => Err("vm is not paused"),
            "PUT /snapshot/create" => {
                let memory = field("mem_file_path").expect("memory path");
                let state = field("snapshot_path").expect("state path");
                if failing == "save" {
                    std::fs::write(memory, "partial").expect("write memory");
                    Err("cannot save")
                } else {
                    std::fs::write(memory, "memory").expect("write memory");
                    std::fs::write(state, "vmstate").expect("write state");
                    Ok(())
                }
            }
            "PUT /snapshot/load" => {
                let memory = std::fs::read_to_string(field("backend_path").expect("memory path"));
                let state = std::fs::read_to_string(field("snapshot_path").expect("state path"));
                let complete = memory.is_ok_and(|memory| memory == "memory")
                    && state.is_ok_and(|state| state == "vmstate")
                    && Path::new("scratch.ext4").is_file()
                    && Path::new("rootfs.ext4").is_file();
                if vm.configured || failing == "load" || !complete {
                    Err("cannot load")
                } else {
                    vm.configured = true;
                    if failing == "block-retire" {
                        std::fs::write("restored", "not a directory").expect("block retire");
                    }
                    Ok(())
                }
            }
            _ => Err("unknown request"),
        }
    };
    let response = match outcome {
        Ok(()) => "HTTP/1.1 204 \r\nServer: Firecracker API\r\n\r\n".to_owned(),
        Err(fault) => {
            let body = format!("{{\"fault_message\":\"{fault}\"}}");
            format!(
                "HTTP/1.1 400 \r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        }
    };
    stream
        .write_all(response.as_bytes())
        .await
        .expect("respond");
    if outcome.is_err() && request == "PUT /snapshot/load" {
        std::process::exit(1);
    }
}
