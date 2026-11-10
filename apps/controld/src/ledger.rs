use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use slop_machines::{ExecutionUsage, Generation, Quotas};

const LEDGER: &str = "ledger.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holdings {
    pub running_machines: u64,
    pub memory_mib: u64,
    pub disk_mib: u64,
}

impl Holdings {
    pub fn running(memory_mib: u32) -> Self {
        Self {
            running_machines: 1,
            memory_mib: memory_mib.into(),
            disk_mib: 0,
        }
    }

    pub fn disk(disk_mib: u64) -> Self {
        Self {
            disk_mib,
            ..Self::default()
        }
    }

    fn add(&mut self, other: Self) {
        self.running_machines += other.running_machines;
        self.memory_mib += other.memory_mib;
        self.disk_mib += other.disk_mib;
    }

    pub fn plus(mut self, other: Self) -> Self {
        self.add(other);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Work {
    Create { request: String },
    Fork { machine: String },
    Snapshot { machine: String },
    Resume { machine: String },
    Pause { machine: String },
    Delete { machine: String },
}

impl Work {
    pub fn machine(&self) -> Option<&str> {
        match self {
            Self::Create { .. } => None,
            Self::Fork { machine }
            | Self::Snapshot { machine }
            | Self::Resume { machine }
            | Self::Pause { machine }
            | Self::Delete { machine } => Some(machine),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operation {
    pub tenant: String,
    pub work: Work,
    pub host: String,
    pub holds: Holdings,
    pub key: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Allocation {
    pub tenant: String,
    pub host: String,
    pub memory_mib: u32,
    pub disk_mib: u64,
    pub running: bool,
    pub key: Option<String>,
}

impl Allocation {
    pub fn holds(&self) -> Holdings {
        let running = if self.running {
            Holdings::running(self.memory_mib)
        } else {
            Holdings::default()
        };
        running.plus(Holdings::disk(self.disk_mib))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotCharge {
    pub tenant: String,
    pub stored_mib: u64,
    pub since_unix_ms: u64,
    pub key: Option<String>,
    pub deleting_since_unix_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub vcpu_ms: u64,
    pub memory_mib_ms: u64,
    pub snapshot_mib_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub host: String,
    pub usage: ExecutionUsage,
    pub acknowledged: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum KeyResult {
    Pending { operation: String },
    Machine { id: String },
    Snapshot { id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Keyed {
    pub request: String,
    pub result: KeyResult,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    pub as_of_unix_ms: u64,
    pub hosts: BTreeMap<String, Generation>,
    pub operations: BTreeMap<String, Operation>,
    pub keys: BTreeMap<String, Keyed>,
    pub machines: BTreeMap<String, Allocation>,
    pub snapshots: BTreeMap<String, SnapshotCharge>,
    pub closed: BTreeMap<String, Usage>,
    pub executions: BTreeMap<String, ExecutionRecord>,
}

pub fn key_of(tenant: &str, key: &str) -> String {
    format!("{tenant}\n{key}")
}

impl Ledger {
    pub async fn load(state_dir: &Path) -> io::Result<Self> {
        match tokio::fs::read(state_dir.join(LEDGER)).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub async fn save(&self, state_dir: &Path) -> io::Result<()> {
        let staged: PathBuf = state_dir.join(format!("{LEDGER}.tmp"));
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        tokio::fs::write(&staged, bytes).await?;
        tokio::fs::File::open(&staged).await?.sync_all().await?;
        tokio::fs::rename(&staged, state_dir.join(LEDGER)).await?;
        tokio::fs::File::open(state_dir).await?.sync_all().await
    }

    pub fn holdings(&self, tenant: &str) -> Holdings {
        let machines = self
            .machines
            .values()
            .filter(|machine| machine.tenant == tenant)
            .map(Allocation::holds);
        let snapshots = self
            .snapshots
            .values()
            .filter(|snapshot| snapshot.tenant == tenant)
            .map(|snapshot| Holdings::disk(snapshot.stored_mib));
        let operations = self
            .operations
            .values()
            .filter(|operation| operation.tenant == tenant)
            .map(|operation| operation.holds);
        machines
            .chain(snapshots)
            .chain(operations)
            .fold(Holdings::default(), Holdings::plus)
    }

    pub fn admits(&self, tenant: &str, quotas: &Quotas, more: Holdings) -> Result<(), String> {
        let after = self.holdings(tenant).plus(more);
        let limits = [
            (
                "running machines",
                after.running_machines,
                u64::from(quotas.running_machines),
                more.running_machines,
            ),
            (
                "running memory MiB",
                after.memory_mib,
                u64::from(quotas.memory_mib),
                more.memory_mib,
            ),
            (
                "disk MiB",
                after.disk_mib,
                u64::from(quotas.disk_gib) << 10,
                more.disk_mib,
            ),
        ];
        for (resource, after, limit, needed) in limits {
            if needed > 0 && after > limit {
                return Err(format!(
                    "the tenant's {resource} quota across the fleet is {limit}; {} is held and this needs {needed}",
                    after - needed
                ));
            }
        }
        Ok(())
    }

    pub fn saw(&mut self, host: &str, generation: Generation) -> bool {
        let seen = self.hosts.entry(host.to_owned()).or_default();
        if generation <= *seen {
            return false;
        }
        *seen = generation;
        true
    }

    pub fn machine_is_busy(&self, machine: &str) -> bool {
        self.operations
            .values()
            .any(|operation| operation.work.machine() == Some(machine))
    }

    pub fn free_key(&mut self, key: Option<&str>) {
        if let Some(key) = key {
            self.keys.remove(key);
        }
    }

    pub fn forget_machine(&mut self, id: &str) {
        if let Some(machine) = self.machines.remove(id) {
            self.free_key(machine.key.as_deref());
        }
    }

    pub fn close_snapshot(&mut self, id: &str, until_unix_ms: u64) {
        if let Some(snapshot) = self.snapshots.remove(id) {
            let until = snapshot.deleting_since_unix_ms.unwrap_or(until_unix_ms);
            let closed = self.closed.entry(snapshot.tenant.clone()).or_default();
            closed.snapshot_mib_ms = closed.snapshot_mib_ms.saturating_add(
                until
                    .saturating_sub(snapshot.since_unix_ms)
                    .saturating_mul(snapshot.stored_mib),
            );
            self.free_key(snapshot.key.as_deref());
        }
    }

    pub fn record_execution(&mut self, host: &str, reported: &ExecutionUsage) {
        let record = self
            .executions
            .entry(reported.id.clone())
            .or_insert_with(|| ExecutionRecord {
                host: host.to_owned(),
                usage: reported.clone(),
                acknowledged: false,
            });
        record.usage.vcpu_ms = record.usage.vcpu_ms.max(reported.vcpu_ms);
        record.usage.memory_mib_ms = record.usage.memory_mib_ms.max(reported.memory_mib_ms);
        record.usage.ended |= reported.ended;
    }

    pub fn fold_execution(&mut self, id: &str) {
        if let Some(record) = self.executions.remove(id) {
            let closed = self.closed.entry(record.usage.tenant).or_default();
            closed.vcpu_ms = closed.vcpu_ms.saturating_add(record.usage.vcpu_ms);
            closed.memory_mib_ms = closed
                .memory_mib_ms
                .saturating_add(record.usage.memory_mib_ms);
        }
    }

    pub fn usage(&self, tenant: &str) -> Usage {
        let mut usage = self.closed.get(tenant).copied().unwrap_or_default();
        for record in self.executions.values() {
            if record.usage.tenant == tenant {
                usage.vcpu_ms = usage.vcpu_ms.saturating_add(record.usage.vcpu_ms);
                usage.memory_mib_ms = usage
                    .memory_mib_ms
                    .saturating_add(record.usage.memory_mib_ms);
            }
        }
        for snapshot in self.snapshots.values() {
            if snapshot.tenant == tenant {
                let until = snapshot
                    .deleting_since_unix_ms
                    .unwrap_or(self.as_of_unix_ms);
                usage.snapshot_mib_ms = usage.snapshot_mib_ms.saturating_add(
                    until
                        .saturating_sub(snapshot.since_unix_ms)
                        .saturating_mul(snapshot.stored_mib),
                );
            }
        }
        usage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quotas() -> Quotas {
        Quotas {
            running_machines: 2,
            memory_mib: 1024,
            disk_gib: 20,
            creates_per_second: 10,
        }
    }

    fn execution(id: &str, vcpu_ms: u64, ended: bool) -> ExecutionUsage {
        ExecutionUsage {
            id: id.into(),
            tenant: "t".into(),
            machine_id: "m".into(),
            vcpu_ms,
            memory_mib_ms: vcpu_ms * 256,
            ended,
        }
    }

    #[test]
    fn quotas_count_machines_snapshots_and_operations_in_flight() {
        let mut ledger = Ledger::default();
        ledger.machines.insert(
            "m".into(),
            Allocation {
                tenant: "t".into(),
                host: "h".into(),
                memory_mib: 512,
                disk_mib: 8704,
                running: true,
                key: None,
            },
        );
        ledger.operations.insert(
            "o".into(),
            Operation {
                tenant: "t".into(),
                work: Work::Resume {
                    machine: "n".into(),
                },
                host: "h".into(),
                holds: Holdings::running(512),
                key: None,
            },
        );
        assert!(
            ledger
                .admits("t", &quotas(), Holdings::running(128))
                .is_err()
        );
        assert!(
            ledger
                .admits("other", &quotas(), Holdings::running(512))
                .is_ok()
        );
        ledger.operations.clear();
        assert!(
            ledger
                .admits("t", &quotas(), Holdings::running(512))
                .is_ok()
        );
        ledger.snapshots.insert(
            "s".into(),
            SnapshotCharge {
                tenant: "t".into(),
                stored_mib: 12 << 10,
                since_unix_ms: 0,
                key: None,
                deleting_since_unix_ms: None,
            },
        );
        assert!(ledger.admits("t", &quotas(), Holdings::disk(1)).is_err());
        assert!(ledger.admits("t", &quotas(), Holdings::default()).is_ok());
    }

    #[test]
    fn repeated_and_reordered_execution_reports_count_once() {
        let mut ledger = Ledger::default();
        ledger.record_execution("h", &execution("e", 100, false));
        ledger.record_execution("h", &execution("e", 300, true));
        ledger.record_execution("h", &execution("e", 200, false));
        assert_eq!(ledger.usage("t").vcpu_ms, 300);
        ledger.fold_execution("e");
        ledger.fold_execution("e");
        assert_eq!(ledger.usage("t").vcpu_ms, 300);
    }

    #[test]
    fn a_snapshot_is_charged_from_publication_until_its_deletion_began() {
        let mut ledger = Ledger {
            as_of_unix_ms: 5_000,
            ..Ledger::default()
        };
        ledger.snapshots.insert(
            "s".into(),
            SnapshotCharge {
                tenant: "t".into(),
                stored_mib: 10,
                since_unix_ms: 1_000,
                key: None,
                deleting_since_unix_ms: None,
            },
        );
        assert_eq!(ledger.usage("t").snapshot_mib_ms, 40_000);
        if let Some(snapshot) = ledger.snapshots.get_mut("s") {
            snapshot.deleting_since_unix_ms = Some(3_000);
        }
        ledger.close_snapshot("s", 9_000);
        assert_eq!(ledger.usage("t").snapshot_mib_ms, 20_000);
    }
}
