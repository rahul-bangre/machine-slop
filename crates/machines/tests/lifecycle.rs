#![cfg(target_os = "linux")]

use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use slop_machines::{
    CreateMachine, CreateSnapshot, IdempotencyKey, Limits, MachineError, MachineInfo, MachineState,
    Machines, Network, Quotas, SnapshotInfo, Tenant, TenantConfig, UpdateMachine,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

const MEMORY_BUDGET_MIB: u32 = 1 << 20;
const DEFAULT_RESERVATION_MIB: u32 = 512 + 64;
const FIRST_JAIL_UID: u32 = 1_000_000;
const TENANT: &str = "tenant";

fn roomy_tenant() -> TenantConfig {
    TenantConfig {
        id: TENANT.into(),
        quotas: Quotas {
            running_machines: 1024,
            memory_mib: MEMORY_BUDGET_MIB,
            disk_gib: 1 << 20,
            creates_per_second: 1 << 20,
        },
    }
}

fn owner(machines: &Machines) -> Arc<Tenant> {
    machines
        .tenant(TENANT)
        .expect("the test tenant is configured")
}

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
            "#!/bin/bash\nFAKE_FIRECRACKER='{}' FAKE_MACHINE_DIR=\"$(dirname \"$0\")\" FAKE_FIRECRACKER_ARGS=\"$*\" exec -a firecracker '{}' --exact fake_firecracker --nocapture\n",
            root.display(),
            std::env::current_exe().expect("test binary").display(),
        );
        std::fs::write(image.join("firecracker"), script).expect("write firecracker");
        let jailer = format!(
            r#"#!/bin/bash
echo "$*" >> '{}/jails'
while [ "$1" != -- ]; do
  case $1 in
    --id) id=$2 ;;
    --exec-file) exec_file=$2 ;;
    --chroot-base-dir) base=$2 ;;
  esac
  shift 2
done
shift
cd "$base/$(basename "$exec_file")/$id/root" && exec "$exec_file" "$@"
"#,
            root.display()
        );
        std::fs::write(image.join("jailer"), jailer).expect("write jailer");
        for program in ["firecracker", "jailer"] {
            std::fs::set_permissions(image.join(program), std::fs::Permissions::from_mode(0o755))
                .expect("make program executable");
        }
        Self { root }
    }

    async fn open(&self, capacity: usize) -> Arc<Machines> {
        self.open_with_spares(capacity, 0).await
    }

    async fn open_with_spares(&self, capacity: usize, spares: usize) -> Arc<Machines> {
        self.open_with(capacity, spares, vec![roomy_tenant()]).await
    }

    async fn open_for(&self, tenants: &[(&str, Quotas)]) -> Arc<Machines> {
        let tenants = tenants
            .iter()
            .map(|(id, quotas)| TenantConfig {
                id: (*id).to_owned(),
                quotas: quotas.clone(),
            })
            .collect();
        self.open_with(8, 0, tenants).await
    }

    async fn open_with(
        &self,
        capacity: usize,
        spares: usize,
        tenants: Vec<TenantConfig>,
    ) -> Arc<Machines> {
        let (image, state) = (self.root.join("image"), self.root.join("state"));
        loop {
            let limits = Limits {
                running_vms: capacity,
                memory_mib: MEMORY_BUDGET_MIB,
                spare_machines: spares,
                first_jail_uid: FIRST_JAIL_UID,
            };
            let tenants = tenants
                .iter()
                .map(|tenant| TenantConfig {
                    id: tenant.id.clone(),
                    quotas: tenant.quotas.clone(),
                })
                .collect();
            match Machines::open(
                image.clone(),
                state.clone(),
                limits,
                tenants,
                Network::default(),
            )
            .await
            {
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

    fn jail_root(&self, id: &str) -> PathBuf {
        let exec_file = format!(
            "--exec-file {} ",
            self.machine_dir(id).join("firecracker").display()
        );
        let jail = self
            .jails()
            .into_iter()
            .rfind(|jail| jail.contains(&exec_file))
            .expect("the machine was jailed");
        let uid = jail.split_whitespace().nth(1).expect("jail id").to_owned();
        self.root
            .join("state/jails/firecracker")
            .join(uid)
            .join("root")
    }

    fn jails(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("jails"))
            .map(|jails| jails.lines().map(str::to_owned).collect())
            .unwrap_or_default()
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
                std::fs::read(process.path().join("environ")).is_ok_and(|environ| {
                    environ
                        .split(|byte| *byte == 0)
                        .filter_map(|variable| variable.strip_prefix(b"FAKE_MACHINE_DIR="))
                        .any(|machine| {
                            Path::new(std::str::from_utf8(machine).unwrap_or("")).starts_with(&dir)
                        })
                })
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
        .create(&owner(machines), CreateMachine::default(), None)
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
    let machine = machines
        .create(&owner(machines), request, None)
        .await
        .expect("create machine");
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
    machines
        .create_snapshot(&owner(machines), request, None)
        .await
}

async fn eventually<F: Future<Output = bool>>(what: &str, done: impl Fn() -> F) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while !done().await {
        assert!(tokio::time::Instant::now() < deadline, "never {what}");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

async fn state(machines: &Machines, id: &str) -> MachineState {
    machines
        .get(&owner(machines), id)
        .await
        .expect("machine")
        .info()
        .state
}

async fn agent_answers(machines: &Machines, id: &str) -> bool {
    let machine = machines.get(&owner(machines), id).await.expect("machine");
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
        .create(&owner(machines), request, None)
        .await
        .expect("create machine")
        .info()
}

async fn egress_allows_localhost(jail_root: &Path) -> bool {
    let mut guest = UnixStream::connect(jail_root.join("vsock.sock_1080"))
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

    assert_eq!(
        states(machines.pause(&owner(&machines), &id).await),
        MachineState::Paused
    );
    assert!(dir.join("checkpoint/vmstate").is_file());
    assert!(dir.join("checkpoint/memory").is_file());
    assert!(!dir.join("checkpoint.tmp").exists());
    assert!(!host.vm_is_alive(&id), "a paused machine has no VM");
    assert!(!agent_answers(&machines, &id).await);
    assert_eq!(
        states(machines.pause(&owner(&machines), &id).await),
        MachineState::Paused
    );

    assert_eq!(
        states(machines.resume(&owner(&machines), &id).await),
        MachineState::Running
    );
    assert!(
        !dir.join("checkpoint").exists(),
        "a running machine has no checkpoint to restore a second time"
    );
    assert!(dir.join("checkpoint.retired/memory").is_file());
    assert!(agent_answers(&machines, &id).await);
    assert_eq!(
        states(machines.resume(&owner(&machines), &id).await),
        MachineState::Running
    );
    assert_eq!(host.vms_started(), 2, "one boot and one restore");

    assert_eq!(
        states(machines.pause(&owner(&machines), &id).await),
        MachineState::Paused
    );
    assert!(dir.join("checkpoint/memory").is_file());
    assert!(!dir.join("checkpoint.retired").exists());
    machines
        .destroy(&owner(&machines), &id)
        .await
        .expect("destroy");
    assert!(!dir.exists());
}

#[tokio::test]
async fn concurrent_resumes_start_one_vm() {
    let host = Host::new("concurrent");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    machines.pause(&owner(&machines), &id).await.expect("pause");

    let resumes: Vec<_> = (0..8)
        .map(|_| {
            let (machines, id) = (machines.clone(), id.clone());
            tokio::spawn(async move { machines.resume(&owner(&machines), &id).await })
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
    machines.pause(&owner(&machines), &id).await.expect("pause");

    host.fail("hold-load");
    let resume = {
        let (machines, id) = (machines.clone(), id.clone());
        tokio::spawn(async move { machines.resume(&owner(&machines), &id).await })
    };
    eventually("started restoring", || async { host.vms_started() == 2 }).await;
    let delete = {
        let (machines, id) = (machines.clone(), id.clone());
        tokio::spawn(async move { machines.destroy(&owner(&machines), &id).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!delete.is_finished(), "the delete waits for the resume");
    assert!(
        machines.get(&owner(&machines), &id).await.is_none(),
        "a machine that is being deleted is already unknown"
    );

    host.fail("nothing");
    assert_eq!(states(resume.await.expect("join")), MachineState::Running);
    delete.await.expect("join").expect("destroy");
    assert!(!host.machine_dir(&id).exists());
    assert_eq!(host.all_vms(), 0);
    for late in [
        machines.resume(&owner(&machines), &id).await,
        machines.pause(&owner(&machines), &id).await,
    ] {
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
        tokio::spawn(async move { machines.delete_snapshot(&owner(&machines), &id).await })
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
    let error = machines
        .pause(&owner(&machines), &id)
        .await
        .expect_err("pause fails");
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
    assert_eq!(
        states(machines.pause(&owner(&machines), &id).await),
        MachineState::Paused
    );
}

#[tokio::test]
async fn a_failed_load_leaves_the_machine_paused() {
    let host = Host::new("load-fails");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    machines.pause(&owner(&machines), &id).await.expect("pause");

    host.fail("load");
    let error = machines
        .resume(&owner(&machines), &id)
        .await
        .expect_err("resume fails");
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
    assert_eq!(
        states(machines.resume(&owner(&machines), &id).await),
        MachineState::Running
    );
}

#[tokio::test]
async fn a_resume_that_fails_once_the_checkpoint_is_retired_loses_the_machine() {
    let host = Host::new("resume-fails");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    machines.pause(&owner(&machines), &id).await.expect("pause");

    host.fail("resume");
    let error = machines
        .resume(&owner(&machines), &id)
        .await
        .expect_err("resume fails");
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
    for retry in [
        machines.resume(&owner(&machines), &id).await,
        machines.pause(&owner(&machines), &id).await,
    ] {
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
    machines
        .pause(&owner(&machines), &paused)
        .await
        .expect("pause");
    let kept = snapshot(&machines, &paused).await.expect("snapshot");
    let resumed = create(&machines).await;
    machines
        .pause(&owner(&machines), &resumed)
        .await
        .expect("pause");
    machines
        .resume(&owner(&machines), &resumed)
        .await
        .expect("resume");
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
    eventually("removed the jails of the earlier run", || async {
        host.all_vms() == 0
    })
    .await;

    assert_eq!(state(&machines, &paused).await, MachineState::Paused);
    for lost in [&running, &resumed] {
        assert!(
            is_exited(&state(&machines, lost).await),
            "memory that was never saved, or was saved before a later run, is not restored"
        );
        assert!(!host.vm_is_alive(lost));
        assert!(matches!(
            machines.resume(&owner(&machines), lost).await,
            Err(MachineError::Conflict(_))
        ));
    }
    assert_eq!(machines.list(&owner(&machines),).await.len(), 3);
    assert!(!unrecorded.exists() && !staging.exists() && !partial_snapshot.exists());
    let snapshots = machines.snapshots(&owner(&machines)).await;
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].id, kept.id);

    assert_eq!(
        states(machines.resume(&owner(&machines), &paused).await),
        MachineState::Running
    );
    create_from(&machines, &kept).await;
}

#[tokio::test]
async fn a_snapshot_outlives_its_source_and_its_deletion_spares_its_machines() {
    let host = Host::new("snapshot");
    let machines = host.open(4).await;
    let source = create(&machines).await;
    machines
        .pause(&owner(&machines), &source)
        .await
        .expect("pause");
    let taken = snapshot(&machines, &source).await.expect("snapshot");
    assert_eq!(taken.machine_id, source);
    let snapshot_dir = host.snapshot_dir(&taken.id);

    let inode = |path: PathBuf| std::fs::metadata(path).expect("metadata").ino();
    assert_ne!(
        inode(snapshot_dir.join("scratch.ext4")),
        inode(host.machine_dir(&source).join("scratch.ext4")),
        "a snapshot's disk is its own"
    );
    machines
        .resume(&owner(&machines), &source)
        .await
        .expect("resume");
    machines
        .destroy(&owner(&machines), &source)
        .await
        .expect("destroy");

    let first = create_from(&machines, &taken).await;
    let second = create_from(&machines, &taken).await;
    let disks = [
        inode(snapshot_dir.join("scratch.ext4")),
        inode(host.machine_dir(&first).join("scratch.ext4")),
        inode(host.machine_dir(&second).join("scratch.ext4")),
    ];
    assert!(disks[0] != disks[1] && disks[0] != disks[2] && disks[1] != disks[2]);

    machines
        .delete_snapshot(&owner(&machines), &taken.id)
        .await
        .expect("delete");
    assert!(!snapshot_dir.exists());
    assert!(machines.snapshots(&owner(&machines),).await.is_empty());
    let request = CreateMachine {
        snapshot_id: Some(taken.id.clone()),
        ..CreateMachine::default()
    };
    assert!(matches!(
        machines.create(&owner(&machines), request, None).await,
        Err(MachineError::UnknownSnapshot)
    ));
    assert!(matches!(
        machines.delete_snapshot(&owner(&machines), &taken.id).await,
        Err(MachineError::UnknownSnapshot)
    ));

    for machine in [&first, &second] {
        assert!(agent_answers(&machines, machine).await);
        assert_eq!(
            states(machines.pause(&owner(&machines), machine).await),
            MachineState::Paused
        );
        assert_eq!(
            states(machines.resume(&owner(&machines), machine).await),
            MachineState::Running
        );
    }
}

#[tokio::test]
async fn capacity_counts_running_vms_and_not_paused_machines() {
    let host = Host::new("capacity");
    let machines = host.open(1).await;
    let parked = create(&machines).await;
    let full = machines
        .create(&owner(&machines), CreateMachine::default(), None)
        .await;
    assert!(matches!(full, Err(MachineError::Capacity)));
    let full = machines.fork(&owner(&machines), &parked, None).await;
    assert!(matches!(full, Err(MachineError::Capacity)));
    assert_eq!(host.vm_pauses(), 0, "a refused fork never stops its source");
    assert!(agent_answers(&machines, &parked).await);

    machines
        .pause(&owner(&machines), &parked)
        .await
        .expect("pause");
    let running = create(&machines).await;
    let full = machines.resume(&owner(&machines), &parked).await;
    assert!(matches!(full, Err(MachineError::Capacity)), "{full:?}");
    assert_eq!(state(&machines, &parked).await, MachineState::Paused);
    assert!(
        host.machine_dir(&parked)
            .join("checkpoint/memory")
            .is_file()
    );

    machines
        .destroy(&owner(&machines), &running)
        .await
        .expect("destroy");
    assert_eq!(
        states(machines.resume(&owner(&machines), &parked).await),
        MachineState::Running
    );
    assert_eq!(host.all_vms(), 1);
}

#[tokio::test]
async fn a_fork_or_snapshot_of_a_running_machine_leaves_it_running() {
    let host = Host::new("fork");
    let machines = host.open(4).await;
    let source = create(&machines).await;

    let fork = machines
        .fork(&owner(&machines), &source, None)
        .await
        .expect("fork");
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

    machines
        .pause(&owner(&machines), &source)
        .await
        .expect("pause");
    let of_paused = machines
        .fork(&owner(&machines), &source, None)
        .await
        .expect("fork");
    assert!(agent_answers(&machines, of_paused.id()).await);
    assert_eq!(state(&machines, &source).await, MachineState::Paused);
    assert_eq!(
        states(machines.resume(&owner(&machines), &source).await),
        MachineState::Running
    );
}

#[tokio::test]
async fn a_failed_fork_or_snapshot_leaves_the_machine_running_and_nothing_else() {
    let host = Host::new("fork-fails");
    let machines = host.open(4).await;
    let source = create(&machines).await;

    host.fail("save");
    let fork = machines
        .fork(&owner(&machines), &source, None)
        .await
        .map(|fork| fork.info());
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
    assert_eq!(machines.list(&owner(&machines),).await.len(), 1);
    assert!(machines.snapshots(&owner(&machines),).await.is_empty());
    let leftovers = |dir: &str| std::fs::read_dir(host.root.join(dir)).expect("dir").count();
    assert_eq!(
        (leftovers("state/machines"), leftovers("state/snapshots")),
        (1, 0)
    );

    host.fail("load");
    let fork = machines
        .fork(&owner(&machines), &source, None)
        .await
        .map(|fork| fork.info());
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
    assert!(
        machines.list(&owner(&machines),).await.is_empty(),
        "a spare is not a machine"
    );

    let first = create(&machines).await;
    assert!(agent_answers(&machines, &first).await);
    eventually("replaced the spare", || async {
        machines.spares().await == 1
    })
    .await;
    assert_eq!(host.vms_started(), 2, "the create booted nothing itself");

    let second = create(&machines).await;
    assert_eq!(host.vms_started(), 2);
    let full = machines
        .create(&owner(&machines), CreateMachine::default(), None)
        .await;
    assert!(matches!(full, Err(MachineError::Capacity)));
    assert_eq!(machines.spares().await, 0);

    machines
        .destroy(&owner(&machines), &second)
        .await
        .expect("destroy");
    eventually("used the freed slot", || async {
        machines.spares().await == 1
    })
    .await;
    let sized = CreateMachine {
        vcpus: Some(1),
        ..CreateMachine::default()
    };
    let sized = machines
        .create(&owner(&machines), sized, None)
        .await
        .expect("create");
    assert_eq!(
        (machines.spares().await, host.all_vms()),
        (0, 2),
        "a spare gives its slot up to a machine it cannot become"
    );
    machines
        .destroy(&owner(&machines), sized.id())
        .await
        .expect("destroy");
    eventually("used the freed slot", || async {
        machines.spares().await == 1
    })
    .await;

    drop((machines, full, sized));
    assert_eq!(host.all_vms(), 2, "machined died without cleanup");
    let machines = host.open(2).await;
    eventually("removed the jails of the earlier run", || async {
        host.all_vms() == 0
    })
    .await;
    assert_eq!(machines.list(&owner(&machines),).await.len(), 1);
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

    machines
        .pause(&owner(&machines), &first)
        .await
        .expect("pause");
    machines
        .pause(&owner(&machines), &second)
        .await
        .expect("pause");
    assert_eq!(
        states(machines.resume(&owner(&machines), &first).await),
        MachineState::Running
    );
    assert_eq!(
        states(machines.resume(&owner(&machines), &second).await),
        MachineState::Running
    );
    let full = machines
        .create(&owner(&machines), CreateMachine::default(), None)
        .await;
    assert!(matches!(full, Err(MachineError::Capacity)));
    assert_eq!((machines.spares().await, host.all_vms()), (0, 2));
}

#[tokio::test]
async fn a_slot_freed_for_a_request_is_not_taken_by_a_new_spare() {
    let host = Host::new("spare-steal");
    let machines = host.open_with_spares(2, 1).await;
    let parked = create(&machines).await;
    let idle = create(&machines).await;
    machines
        .pause(&owner(&machines), &parked)
        .await
        .expect("pause");
    machines
        .pause(&owner(&machines), &idle)
        .await
        .expect("pause");
    let running = create(&machines).await;
    eventually("booted a spare", || async { machines.spares().await == 1 }).await;

    let noise = {
        let (machines, idle) = (machines.clone(), idle.clone());
        tokio::spawn(async move {
            loop {
                machines
                    .pause(&owner(&machines), &idle)
                    .await
                    .expect("pause");
            }
        })
    };
    let started = host.vms_started();
    let tenant = owner(&machines);
    let resume = machines.resume(&tenant, &parked);
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

    machines
        .destroy(&owner(&machines), &running)
        .await
        .expect("destroy");
    eventually("booted a spare again", || async {
        machines.spares().await == 1
    })
    .await;
    let exited = create(&machines).await;
    host.fail("resume");
    machines
        .pause(&owner(&machines), &exited)
        .await
        .expect("pause");
    machines
        .resume(&owner(&machines), &exited)
        .await
        .expect_err("resume fails");
    host.fail("nothing");
    eventually("booted a spare again", || async {
        machines.spares().await == 1
    })
    .await;
    let fork = machines
        .fork(&owner(&machines), &exited, None)
        .await
        .map(|fork| fork.info());
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
        tokio::spawn(async move { machines.pause(&owner(&machines), &id).await })
    };
    let staged = host.jail_root(&id).join("memory");
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
    machines.pause(&owner(&machines), &id).await.expect("pause");
    let dir = host.machine_dir(&id);

    host.fail("block-retire");
    let error = machines
        .resume(&owner(&machines), &id)
        .await
        .expect_err("resume fails");
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
    std::fs::remove_file(dir.join("checkpoint.retired")).expect("unblock retire");
    assert_eq!(
        states(machines.resume(&owner(&machines), &id).await),
        MachineState::Running
    );
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
    let id = machines
        .create(&owner(&machines), request, None)
        .await
        .expect("create")
        .info()
        .id;
    let dir = host.machine_dir(&id);
    let set_mode = |mode| {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
    };
    let _writable_again_for_cleanup = WritableOnDrop(dir.clone());

    set_mode(0o555);
    eventually("listed again, exited, after a failed expiry", || async {
        let listed = machines
            .get(&owner(&machines), &id)
            .await
            .map(|machine| machine.info().state);
        listed.as_ref().is_some_and(is_exited)
    })
    .await;
    let half_the_idle_timeout = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    while tokio::time::Instant::now() < half_the_idle_timeout {
        assert!(
            machines.get(&owner(&machines), &id).await.is_some(),
            "a failed expiry waits a full idle timeout before trying again"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    set_mode(0o755);
    assert!(dir.join("machine.json").is_file());
    assert!(!host.vm_is_alive(&id));

    eventually("expired once its record could go", || async {
        machines.get(&owner(&machines), &id).await.is_none() && !dir.exists()
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
    machines.pause(&owner(&machines), &id).await.expect("pause");
    let dir = host.machine_dir(&id);
    let set_mode = |mode| {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
    };
    let _writable_again_for_cleanup = WritableOnDrop(dir.clone());

    set_mode(0o555);
    let failed = machines.destroy(&owner(&machines), &id).await;
    set_mode(0o755);
    assert!(matches!(failed, Err(MachineError::Io(_))), "{failed:?}");
    assert_eq!(state(&machines, &id).await, MachineState::Paused);

    drop(machines);
    let machines = host.open(4).await;
    assert_eq!(
        states(machines.resume(&owner(&machines), &id).await),
        MachineState::Running
    );
    machines
        .destroy(&owner(&machines), &id)
        .await
        .expect("destroy");
    assert!(!dir.exists());
}

#[tokio::test]
async fn allowed_hosts_survive_pause_and_restart_and_pass_to_forks_and_snapshots() {
    let host = Host::new("egress");
    let machines = host.open(8).await;
    let source = create_allowing(&machines, &["localhost"]).await.id;
    assert!(egress_allows_localhost(&host.jail_root(&source)).await);

    machines
        .pause(&owner(&machines), &source)
        .await
        .expect("pause");
    let narrowed = UpdateMachine {
        allowed_hosts: Some(vec!["example.com".into()]),
        egress_credentials: None,
    };
    let updated = machines
        .update(&owner(&machines), &source, narrowed)
        .await
        .expect("update");
    assert_eq!(updated.allowed_hosts, ["example.com"]);
    drop(machines);
    let machines = host.open(8).await;
    assert_eq!(
        states(machines.resume(&owner(&machines), &source).await),
        MachineState::Running
    );
    let resumed = machines
        .get(&owner(&machines), &source)
        .await
        .expect("machine")
        .info();
    assert_eq!(resumed.allowed_hosts, ["example.com"]);
    assert!(!egress_allows_localhost(&host.jail_root(&source)).await);

    let fork = machines
        .fork(&owner(&machines), &source, None)
        .await
        .expect("fork")
        .info();
    assert_eq!(fork.allowed_hosts, ["example.com"]);
    assert_eq!(fork.idle_timeout_secs, resumed.idle_timeout_secs);
    assert!(!egress_allows_localhost(&host.jail_root(&fork.id)).await);

    machines
        .pause(&owner(&machines), &source)
        .await
        .expect("pause");
    host.fail("hold-load");
    let started = host.vms_started();
    let resuming = {
        let (machines, source) = (machines.clone(), source.clone());
        tokio::spawn(async move { machines.resume(&owner(&machines), &source).await })
    };
    eventually("started restoring", || async {
        host.vms_started() > started
    })
    .await;
    let revoking = {
        let (machines, source) = (machines.clone(), source.clone());
        let none = UpdateMachine {
            allowed_hosts: Some(Vec::new()),
            egress_credentials: None,
        };
        tokio::spawn(async move { machines.update(&owner(&machines), &source, none).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let (forking, snapshotting) = {
        let (forks, snapshots, source) = (machines.clone(), machines.clone(), source.clone());
        let fork_source = source.clone();
        (
            tokio::spawn(async move {
                forks
                    .fork(&owner(&forks), &fork_source, None)
                    .await
                    .map(|fork| fork.info())
            }),
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
        allowed_hosts: Some(vec!["example.com".into()]),
        egress_credentials: None,
    };
    machines
        .update(&owner(&machines), &source, restored)
        .await
        .expect("update");

    let taken = snapshot(&machines, &source).await.expect("snapshot");
    assert_eq!(taken.allowed_hosts, ["example.com"]);
    let inheriting = create_from(&machines, &taken).await;
    assert!(!egress_allows_localhost(&host.jail_root(&inheriting)).await);
    let overriding = CreateMachine {
        snapshot_id: Some(taken.id.clone()),
        allowed_hosts: Some(vec!["localhost".into()]),
        ..CreateMachine::default()
    };
    let overriding = machines
        .create(&owner(&machines), overriding, None)
        .await
        .expect("create")
        .info();
    assert_eq!(overriding.allowed_hosts, ["localhost"]);
    assert!(egress_allows_localhost(&host.jail_root(&overriding.id)).await);
}

#[tokio::test]
async fn a_spare_has_no_egress_until_a_create_gives_it_that_machines_list() {
    let host = Host::new("spare-egress");
    let machines = host.open_with_spares(2, 1).await;
    eventually("booted a spare", || async { machines.spares().await == 1 }).await;
    let spare = std::fs::read_dir(host.root.join("state/machines"))
        .expect("machines")
        .next()
        .expect("the spare's directory")
        .expect("entry")
        .file_name()
        .into_string()
        .expect("spare id");
    assert!(!egress_allows_localhost(&host.jail_root(&spare)).await);

    let taken = create_allowing(&machines, &["localhost"]).await;
    assert_eq!(taken.id, spare);
    assert_eq!(taken.allowed_hosts, ["localhost"]);
    assert!(egress_allows_localhost(&host.jail_root(&spare)).await);
}

#[tokio::test]
async fn a_paused_machine_does_not_expire_and_a_resumed_one_does() {
    let host = Host::new("idle");
    let machines = host.open(4).await;
    let request = CreateMachine {
        idle_timeout_secs: Some(1),
        ..CreateMachine::default()
    };
    let id = machines
        .create(&owner(&machines), request, None)
        .await
        .expect("create")
        .info()
        .id;
    machines.pause(&owner(&machines), &id).await.expect("pause");
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert_eq!(state(&machines, &id).await, MachineState::Paused);

    drop(machines);
    let machines = host.open(4).await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(state(&machines, &id).await, MachineState::Paused);

    assert_eq!(
        states(machines.resume(&owner(&machines), &id).await),
        MachineState::Running
    );
    eventually("expired", || async {
        machines.get(&owner(&machines), &id).await.is_none()
    })
    .await;
    eventually("removed", || async { !host.machine_dir(&id).exists() }).await;
    assert_eq!(host.all_vms(), 0);

    let request = CreateMachine {
        idle_timeout_secs: Some(1),
        ..CreateMachine::default()
    };
    let lost = machines
        .create(&owner(&machines), request, None)
        .await
        .expect("create")
        .info()
        .id;
    machines
        .pause(&owner(&machines), &lost)
        .await
        .expect("pause");
    host.fail("resume");
    machines
        .resume(&owner(&machines), &lost)
        .await
        .expect_err("resume fails");
    assert!(is_exited(&state(&machines, &lost).await));
    eventually("expired once it was no longer paused", || async {
        machines.get(&owner(&machines), &lost).await.is_none()
    })
    .await;
}

#[tokio::test]
async fn shutdown_pauses_running_machines_and_refuses_to_start_more() {
    let host = Host::new("shutdown");
    let machines = host.open_with_spares(4, 1).await;
    let running = create(&machines).await;
    let paused = create(&machines).await;
    machines
        .pause(&owner(&machines), &paused)
        .await
        .expect("pause");
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
        tokio::spawn(async move {
            machines
                .create(&owner(&machines), request, None)
                .await
                .map(|machine| machine.info())
        })
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
    assert_eq!(machines.list(&owner(&machines),).await.len(), 2);
    assert_eq!(
        machines.resource_usage(),
        (0, 0, MEMORY_BUDGET_MIB),
        "every reservation is back once shutdown returns"
    );
    for id in [&running, &paused] {
        assert_eq!(state(&machines, id).await, MachineState::Paused);
        assert!(!host.vm_is_alive(id));
        assert!(matches!(
            machines.resume(&owner(&machines), id).await,
            Err(MachineError::ShuttingDown)
        ));
    }
    assert!(matches!(
        machines
            .create(&owner(&machines), CreateMachine::default(), None)
            .await,
        Err(MachineError::ShuttingDown)
    ));
    assert!(matches!(
        machines.fork(&owner(&machines), &paused, None).await,
        Err(MachineError::ShuttingDown)
    ));

    drop(machines);
    let machines = host.open(4).await;
    for id in [&running, &paused] {
        assert_eq!(
            states(machines.resume(&owner(&machines), id).await),
            MachineState::Running
        );
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
            .all(|id| host.jail_root(id).join("memory").is_file())
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
        first_jail_uid: FIRST_JAIL_UID,
    };
    let machines = Machines::open(
        image,
        state,
        limits,
        vec![roomy_tenant()],
        Network::default(),
    )
    .await
    .expect("open");
    eventually("booted a spare", || async { machines.spares().await == 1 }).await;
    assert_eq!(
        machines.resource_usage(),
        (1, DEFAULT_RESERVATION_MIB, budget)
    );

    let sized = CreateMachine {
        memory_mib: Some(2048),
        ..CreateMachine::default()
    };
    let refused = machines
        .create(&owner(&machines), sized, None)
        .await
        .map(|machine| machine.info());
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
        .create(
            &owner(&machines),
            CreateMachine {
                vcpus: Some(1),
                ..CreateMachine::default()
            },
            None,
        )
        .await
        .expect("a spare gives its memory up to a machine it cannot become")
        .info()
        .id;
    assert_eq!(machines.spares().await, 0);
    assert_eq!(machines.resource_usage(), (2, budget, budget));
    let full = machines
        .create(&owner(&machines), CreateMachine::default(), None)
        .await;
    assert!(matches!(full, Err(MachineError::Capacity)));
    assert_eq!(
        std::fs::read_dir(host.root.join("state/machines"))
            .expect("machines")
            .count(),
        2,
        "a refused create leaves no files"
    );

    machines
        .pause(&owner(&machines), &first)
        .await
        .expect("pause");
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
    machines
        .destroy(&owner(&machines), &second)
        .await
        .expect("destroy");
    assert_eq!(
        states(machines.resume(&owner(&machines), &first).await),
        MachineState::Running
    );
    machines
        .destroy(&owner(&machines), &first)
        .await
        .expect("destroy");
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
    machines.pause(&owner(&machines), &id).await.expect("pause");
    let taken = snapshot(&machines, &id).await.expect("snapshot");

    let resized = CreateMachine {
        snapshot_id: Some(taken.id),
        vcpus: Some(1),
        ..CreateMachine::default()
    };
    assert!(matches!(
        machines.create(&owner(&machines), resized, None).await,
        Err(MachineError::Invalid(_))
    ));
    let missing = CreateMachine {
        snapshot_id: Some("missing".into()),
        ..CreateMachine::default()
    };
    assert!(matches!(
        machines.create(&owner(&machines), missing, None).await,
        Err(MachineError::UnknownSnapshot)
    ));
    assert!(matches!(
        machines.pause(&owner(&machines), "missing").await,
        Err(MachineError::UnknownMachine)
    ));
    assert!(matches!(
        machines.resume(&owner(&machines), "missing").await,
        Err(MachineError::UnknownMachine)
    ));
    assert!(matches!(
        snapshot(&machines, "missing").await,
        Err(MachineError::UnknownMachine)
    ));
    assert!(matches!(
        machines.fork(&owner(&machines), "missing", None).await,
        Err(MachineError::UnknownMachine)
    ));
    assert!(matches!(
        machines.destroy(&owner(&machines), "missing").await,
        Err(MachineError::UnknownMachine)
    ));
    assert_eq!(host.vms_started(), 1);
}

fn quotas(running_machines: u32, memory_mib: u32, disk_gib: u32) -> Quotas {
    Quotas {
        running_machines,
        memory_mib,
        disk_gib,
        creates_per_second: 1000,
    }
}

fn key(key: &str) -> Option<IdempotencyKey> {
    Some(IdempotencyKey::parse(key).expect("valid key"))
}

fn jail_argument<'a>(jail: &'a str, flag: &str) -> Vec<&'a str> {
    let words: Vec<_> = jail.split_whitespace().collect();
    words
        .windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1])
        .collect()
}

#[tokio::test]
async fn every_running_vm_is_jailed_as_its_own_user_with_its_reservation_as_limits() {
    let host = Host::new("jail");
    let machines = host.open(2).await;
    let first = create(&machines).await;
    let sized = CreateMachine {
        vcpus: Some(1),
        memory_mib: Some(256),
        ..CreateMachine::default()
    };
    let second = machines
        .create(&owner(&machines), sized, None)
        .await
        .expect("create")
        .id()
        .to_owned();

    let jails = host.jails();
    assert_eq!(jails.len(), 2);
    let uids: Vec<_> = jails
        .iter()
        .map(|jail| jail_argument(jail, "--uid")[0])
        .collect();
    assert_ne!(uids[0], uids[1], "no two running VMs share a user");
    for (jail, uid) in jails.iter().zip(&uids) {
        assert_eq!(jail_argument(jail, "--gid"), [*uid]);
        assert_eq!(jail_argument(jail, "--id"), [*uid]);
        assert!(uid.parse::<u32>().expect("numeric uid") >= FIRST_JAIL_UID);
        assert_eq!(jail_argument(jail, "--cgroup-version"), ["2"]);
    }
    let limits = |jail: &str| jail_argument(jail, "--cgroup").join(" ");
    assert!(limits(&jails[0]).contains(&format!("memory.max={}", 576u64 << 20)));
    assert!(limits(&jails[0]).contains("cpu.max=200000"));
    assert!(limits(&jails[1]).contains(&format!("memory.max={}", 320u64 << 20)));
    assert!(limits(&jails[1]).contains("cpu.max=100000"));

    let root = host.jail_root(&first);
    let jailed: Vec<_> = std::fs::read_dir(&root)
        .expect("jail root")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("name")
        })
        .filter(|name| !name.ends_with(".sock") && !name.starts_with("vsock.sock_"))
        .collect();
    let mut jailed = jailed;
    jailed.sort();
    assert_eq!(
        jailed,
        ["config.json", "rootfs.ext4", "scratch.ext4", "vmlinux"],
        "the jail holds only this machine's files"
    );
    let scratch = |dir: &Path| {
        std::fs::metadata(dir.join("scratch.ext4"))
            .expect("disk")
            .ino()
    };
    assert_eq!(scratch(&root), scratch(&host.machine_dir(&first)));

    assert_eq!(
        states(machines.pause(&owner(&machines), &first).await),
        MachineState::Paused
    );
    assert!(!root.exists(), "a VM's jail is removed when it exits");
    let checkpoint = host.machine_dir(&first).join("checkpoint/memory");
    let sealed = std::fs::metadata(&checkpoint).expect("checkpoint");
    assert_eq!(sealed.permissions().mode() & 0o777, 0o444);
    assert_eq!(
        sealed.uid(),
        std::fs::metadata(&host.root).expect("root").uid()
    );

    machines
        .destroy(&owner(&machines), &second)
        .await
        .expect("destroy");
    machines
        .resume(&owner(&machines), &first)
        .await
        .expect("resume");
    let resumed = host.jails().pop().expect("resumed jail");
    assert!(
        uids.contains(&jail_argument(&resumed, "--uid")[0]),
        "uids come from a fixed pool"
    );
}

#[tokio::test]
async fn a_vm_cannot_redirect_machined_to_another_machine_through_its_jail() {
    let host = Host::new("jail-sockets");
    let machines = host.open(4).await;
    let (first, second) = (create(&machines).await, create(&machines).await);
    for socket in ["vsock.sock", "api.sock"] {
        let planted = host.jail_root(&first).join(socket);
        std::fs::remove_file(&planted).expect("remove socket");
        std::os::unix::fs::symlink(host.jail_root(&second).join(socket), &planted)
            .expect("plant symlink");
    }
    assert!(!agent_answers(&machines, &first).await);
    assert!(agent_answers(&machines, &second).await);
    let paused = machines.pause(&owner(&machines), &first).await;
    assert!(
        matches!(paused, Err(MachineError::Failed { .. })),
        "{paused:?}"
    );
    assert_eq!(state(&machines, &second).await, MachineState::Running);
    assert_eq!(host.vm_pauses(), 0, "no request reached the other VM");
}

#[tokio::test]
async fn saved_state_that_is_not_a_file_the_vm_wrote_is_refused_untouched() {
    let host = Host::new("jail-files");
    let machines = host.open(4).await;
    let id = create(&machines).await;
    let host_file = host.root.join("host-file");
    std::fs::write(&host_file, "host").expect("write host file");
    std::fs::set_permissions(&host_file, std::fs::Permissions::from_mode(0o640)).expect("set mode");

    host.fail("save-as-symlink");
    let paused = machines.pause(&owner(&machines), &id).await;
    assert!(
        matches!(paused, Err(MachineError::Failed { .. })),
        "{paused:?}"
    );
    let untouched = std::fs::metadata(&host_file).expect("host file");
    assert_eq!(untouched.permissions().mode() & 0o777, 0o640);
    assert_eq!(std::fs::read_to_string(&host_file).expect("read"), "host");
    assert!(!host.machine_dir(&id).join("checkpoint").exists());
    assert_eq!(state(&machines, &id).await, MachineState::Running);
    let snapshot = snapshot(&machines, &id).await;
    assert!(snapshot.is_err(), "{snapshot:?}");
    assert!(machines.snapshots(&owner(&machines)).await.is_empty());
}

#[tokio::test]
async fn tenants_see_and_change_only_their_own_machines_and_snapshots() {
    let host = Host::new("tenants");
    let roomy = quotas(8, 8192, 1024);
    let machines = host.open_for(&[("a", roomy.clone()), ("b", roomy)]).await;
    let (a, b) = (
        machines.tenant("a").expect("a"),
        machines.tenant("b").expect("b"),
    );
    assert!(machines.tenant("c").is_none());
    let machine = machines
        .create(&a, CreateMachine::default(), None)
        .await
        .expect("create")
        .id()
        .to_owned();
    let snapshot = machines
        .create_snapshot(
            &a,
            CreateSnapshot {
                machine_id: machine.clone(),
            },
            None,
        )
        .await
        .expect("snapshot");

    assert!(machines.get(&b, &machine).await.is_none());
    assert!(machines.begin_use(&b, &machine).await.is_none());
    assert!(machines.list(&b).await.is_empty());
    assert!(machines.snapshots(&b).await.is_empty());
    assert!(machines.snapshot_info(&b, &snapshot.id).await.is_none());
    for refused in [
        machines.pause(&b, &machine).await.map(|_| ()),
        machines.resume(&b, &machine).await.map(|_| ()),
        machines.fork(&b, &machine, None).await.map(|_| ()),
        machines.destroy(&b, &machine).await,
        machines
            .update(
                &b,
                &machine,
                UpdateMachine {
                    allowed_hosts: Some(vec![]),
                    egress_credentials: None,
                },
            )
            .await
            .map(|_| ()),
        machines
            .create_snapshot(
                &b,
                CreateSnapshot {
                    machine_id: machine.clone(),
                },
                None,
            )
            .await
            .map(|_| ()),
    ] {
        assert!(
            matches!(refused, Err(MachineError::UnknownMachine)),
            "{refused:?}"
        );
    }
    let from_snapshot = CreateMachine {
        snapshot_id: Some(snapshot.id.clone()),
        ..CreateMachine::default()
    };
    assert!(matches!(
        machines.create(&b, from_snapshot, None).await,
        Err(MachineError::UnknownSnapshot)
    ));
    assert!(matches!(
        machines.delete_snapshot(&b, &snapshot.id).await,
        Err(MachineError::UnknownSnapshot)
    ));
    assert_eq!(
        states(machines.pause(&a, &machine).await),
        MachineState::Paused
    );
    assert_eq!(machines.list(&a).await.len(), 1);

    drop(machines);
    let machines = host.open_for(&[("b", quotas(8, 8192, 1024))]).await;
    let b = machines.tenant("b").expect("b");
    assert!(
        machines.list(&b).await.is_empty(),
        "a removed tenant's machines stay its own"
    );
    assert!(machines.tenant("a").is_some(), "and are kept for it");
    assert_eq!(machines.states().await, [MachineState::Paused]);
}

#[tokio::test]
async fn quotas_refuse_work_before_it_starts_and_free_up_as_it_ends() {
    let host = Host::new("quotas");
    let machines = host.open_for(&[("t", quotas(1, 1024, 20))]).await;
    let tenant = machines.tenant("t").expect("tenant");
    let first = machines
        .create(&tenant, CreateMachine::default(), None)
        .await
        .expect("create")
        .id()
        .to_owned();
    let started = host.vms_started();

    let refused = machines
        .create(&tenant, CreateMachine::default(), None)
        .await
        .map(|machine| machine.info());
    assert!(
        matches!(refused, Err(MachineError::QuotaExceeded(_))),
        "{refused:?}"
    );
    let refused = machines
        .fork(&tenant, &first, None)
        .await
        .map(|machine| machine.info());
    assert!(
        matches!(refused, Err(MachineError::QuotaExceeded(_))),
        "{refused:?}"
    );
    assert_eq!(
        host.vms_started(),
        started,
        "nothing started for a refused request"
    );
    assert_eq!(machines.list(&tenant).await.len(), 1);

    machines.pause(&tenant, &first).await.expect("pause");
    let second = machines
        .create(&tenant, CreateMachine::default(), None)
        .await
        .expect("a paused machine runs no VM")
        .id()
        .to_owned();
    let refused = machines.resume(&tenant, &first).await;
    assert!(
        matches!(refused, Err(MachineError::QuotaExceeded(_))),
        "{refused:?}"
    );
    let first_state = machines
        .get(&tenant, &first)
        .await
        .expect("first")
        .info()
        .state;
    assert_eq!(first_state, MachineState::Paused);
    machines.pause(&tenant, &second).await.expect("pause");

    let too_big = CreateMachine {
        memory_mib: Some(2048),
        ..CreateMachine::default()
    };
    let refused = machines
        .create(&tenant, too_big, None)
        .await
        .map(|machine| machine.info());
    assert!(
        matches!(refused, Err(MachineError::QuotaExceeded(_))),
        "{refused:?}"
    );

    let refused = machines
        .create(&tenant, CreateMachine::default(), None)
        .await
        .map(|machine| machine.info());
    assert!(
        matches!(refused, Err(MachineError::QuotaExceeded(_))),
        "two 8.5 GiB machines leave no room for a third in 20 GiB: {refused:?}"
    );
    machines.destroy(&tenant, &second).await.expect("destroy");
    let snapshot = machines
        .create_snapshot(
            &tenant,
            CreateSnapshot {
                machine_id: first.clone(),
            },
            None,
        )
        .await
        .expect("a snapshot counts what it stores");
    assert!(snapshot.stored_bytes > 0);
    machines.resume(&tenant, &first).await.expect("resume");
}

#[tokio::test]
async fn creates_beyond_the_tenant_rate_are_refused_until_it_refills() {
    let host = Host::new("rate");
    let limited = Quotas {
        creates_per_second: 2,
        ..quotas(8, 8192, 1024)
    };
    let machines = host.open_for(&[("t", limited)]).await;
    let tenant = machines.tenant("t").expect("tenant");
    for _ in 0..2 {
        machines
            .create(&tenant, CreateMachine::default(), None)
            .await
            .expect("within the burst");
    }
    let started = host.vms_started();
    let Err(MachineError::RateLimited { retry_after }) = machines
        .create(&tenant, CreateMachine::default(), None)
        .await
    else {
        panic!("a third create in the same second is limited");
    };
    assert_eq!(host.vms_started(), started);
    tokio::time::sleep(retry_after).await;
    machines
        .create(&tenant, CreateMachine::default(), None)
        .await
        .expect("refilled");
}

#[tokio::test]
async fn a_retried_create_fork_or_snapshot_returns_what_its_key_already_made() {
    let host = Host::new("idempotent");
    let machines = host.open(8).await;
    let tenant = owner(&machines);
    let attempts: Vec<_> = (0..3)
        .map(|_| {
            let (machines, tenant) = (machines.clone(), tenant.clone());
            tokio::spawn(async move {
                machines
                    .create(&tenant, CreateMachine::default(), key("create-1"))
                    .await
                    .map(|machine| machine.id().to_owned())
            })
        })
        .collect();
    let mut created = Vec::new();
    for attempt in attempts {
        created.push(attempt.await.expect("join").expect("create"));
    }
    assert!(created.iter().all(|id| *id == created[0]), "{created:?}");
    assert_eq!(host.vms_started(), 1, "concurrent retries started one VM");
    let machine = created[0].clone();

    let different = CreateMachine {
        vcpus: Some(1),
        ..CreateMachine::default()
    };
    assert!(matches!(
        machines.create(&tenant, different, key("create-1")).await,
        Err(MachineError::Invalid(_))
    ));
    assert!(matches!(
        machines.fork(&tenant, &machine, key("create-1")).await,
        Err(MachineError::Invalid(_))
    ));

    let fork = machines
        .fork(&tenant, &machine, key("fork-1"))
        .await
        .expect("fork");
    let again = machines
        .fork(&tenant, &machine, key("fork-1"))
        .await
        .expect("fork");
    assert_eq!(fork.id(), again.id());
    let snapshot = machines
        .create_snapshot(
            &tenant,
            CreateSnapshot {
                machine_id: machine.clone(),
            },
            key("snap-1"),
        )
        .await
        .expect("snapshot");
    let started = host.vms_started();

    drop((machines, tenant, fork, again));
    let machines = host.open(8).await;
    let tenant = owner(&machines);
    let replayed = machines
        .create(&tenant, CreateMachine::default(), key("create-1"))
        .await
        .expect("replayed after a restart");
    assert_eq!(replayed.id(), machine);
    assert!(
        is_exited(&replayed.info().state),
        "it reports what the machine is now"
    );
    let replayed = machines
        .create_snapshot(
            &tenant,
            CreateSnapshot {
                machine_id: machine.clone(),
            },
            key("snap-1"),
        )
        .await
        .expect("replayed snapshot");
    assert_eq!(replayed.id, snapshot.id);
    assert_eq!(host.vms_started(), started, "a replay starts nothing");

    machines.destroy(&tenant, &machine).await.expect("destroy");
    let fresh = machines
        .create(&tenant, CreateMachine::default(), key("create-1"))
        .await
        .expect("a key whose machine is gone makes a new one");
    assert_ne!(fresh.id(), machine);
}

#[tokio::test]
async fn usage_is_recorded_durably_and_never_goes_down() {
    let host = Host::new("usage");
    let machines = host.open(8).await;
    let tenant = owner(&machines);
    let before = tenant.usage();
    assert_eq!(
        (before.vcpu_ms, before.memory_mib_ms, before.snapshot_mib_ms),
        (0, 0, 0)
    );
    let machine = create(&machines).await;
    let snapshot = snapshot(&machines, &machine).await.expect("snapshot");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    machines.pause(&tenant, &machine).await.expect("pause");
    machines.shutdown().await;

    let recorded = tenant.usage();
    assert!(recorded.vcpu_ms >= 2 * 300, "{recorded:?}");
    assert_eq!(recorded.memory_mib_ms, recorded.vcpu_ms / 2 * 512);
    assert!(recorded.snapshot_mib_ms > 0, "{recorded:?}");
    let paused_since = recorded.as_of_unix_ms;

    drop((machines, tenant));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let machines = host.open(8).await;
    let tenant = owner(&machines);
    let reopened = tenant.usage();
    assert_eq!(
        (
            reopened.vcpu_ms,
            reopened.snapshot_mib_ms,
            reopened.as_of_unix_ms
        ),
        (recorded.vcpu_ms, recorded.snapshot_mib_ms, paused_since),
        "a restart reports what was recorded"
    );
    machines.shutdown().await;
    let after = tenant.usage();
    assert_eq!(
        after.vcpu_ms, recorded.vcpu_ms,
        "a paused machine accrues no VM time"
    );
    let stored_mib = snapshot.stored_bytes.div_ceil(1 << 20);
    assert!(
        after.snapshot_mib_ms >= recorded.snapshot_mib_ms + stored_mib * 200,
        "a snapshot accrues storage while machined is down: {after:?}"
    );
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
    tokio::spawn(async {
        while std::env::current_dir().is_ok() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        std::process::exit(0);
    });
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

fn machine_dir() -> PathBuf {
    PathBuf::from(std::env::var("FAKE_MACHINE_DIR").expect("machine dir"))
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
            "PATCH /vm" if machine_dir().join("checkpoint").exists() => {
                Err("checkpoint is published")
            }
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
                } else if failing == "save-as-symlink" {
                    std::os::unix::fs::symlink(fail.with_file_name("host-file"), memory)
                        .expect("plant symlink");
                    std::fs::write(state, "vmstate").expect("write state");
                    Ok(())
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
                        std::fs::write(machine_dir().join("checkpoint.retired"), "not a directory")
                            .expect("block retire");
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
