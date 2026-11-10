use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Default, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CreateMachine {
    #[serde(default)]
    #[ts(optional = nullable)]
    pub vcpus: Option<u8>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub memory_mib: Option<u32>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub idle_timeout_secs: Option<u32>,
    #[serde(default, deserialize_with = "listed")]
    #[ts(optional)]
    pub allowed_hosts: Option<Vec<String>>,
    #[serde(default, deserialize_with = "listed")]
    #[ts(optional)]
    pub egress_credentials: Option<Vec<EgressCredential>>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub egress_address: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub snapshot_id: Option<String>,
}

fn listed<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Vec<T>>, D::Error> {
    Vec::deserialize(deserializer).map(Some)
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct EgressCredential {
    pub host: String,
    pub headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for EgressCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressCredential")
            .field("host", &self.host)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Debug, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct UpdateMachine {
    #[serde(default, deserialize_with = "listed")]
    #[ts(optional)]
    pub allowed_hosts: Option<Vec<String>>,
    #[serde(default, deserialize_with = "listed")]
    #[ts(optional)]
    pub egress_credentials: Option<Vec<EgressCredential>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, TS)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MachineState {
    Running,
    Paused,
    Exited { message: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct MachineInfo {
    pub id: String,
    pub state: MachineState,
    pub vcpus: u8,
    pub memory_mib: u32,
    #[ts(type = "number")]
    pub boot_ms: u64,
    pub idle_timeout_secs: u32,
    pub allowed_hosts: Vec<String>,
    pub credential_hosts: Vec<String>,
    pub egress_address: Option<String>,
}

#[derive(Debug, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CreateSnapshot {
    pub machine_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct SnapshotInfo {
    pub id: String,
    pub machine_id: String,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub allowed_hosts: Vec<String>,
    #[ts(type = "number")]
    pub created_unix_ms: u64,
    #[ts(type = "number")]
    pub stored_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub pool: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Quotas {
    pub running_machines: u32,
    pub memory_mib: u32,
    pub disk_gib: u32,
    pub creates_per_second: u32,
}

impl Quotas {
    pub fn unenforced() -> Self {
        Self {
            running_machines: u32::MAX,
            memory_mib: u32::MAX,
            disk_gib: u32::MAX,
            creates_per_second: u32::MAX,
        }
    }

    pub fn none() -> Self {
        Self {
            running_machines: 0,
            memory_mib: 0,
            disk_gib: 0,
            creates_per_second: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct TenantUsage {
    pub tenant: String,
    pub quotas: Quotas,
    #[ts(type = "number")]
    pub vcpu_ms: u64,
    #[ts(type = "number")]
    pub memory_mib_ms: u64,
    #[ts(type = "number")]
    pub snapshot_mib_ms: u64,
    #[ts(type = "number")]
    pub as_of_unix_ms: u64,
}

#[derive(Debug, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CreatePreview {
    pub machine_id: String,
    pub port: u16,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub ttl_secs: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct PreviewInfo {
    pub id: String,
    pub machine_id: String,
    pub port: u16,
    pub url: String,
    #[ts(type = "number")]
    pub expires_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, TS)]
pub struct ExecutionUsage {
    pub id: String,
    pub tenant: String,
    pub machine_id: String,
    #[ts(type = "number")]
    pub vcpu_ms: u64,
    #[ts(type = "number")]
    pub memory_mib_ms: u64,
    pub ended: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, TS)]
pub struct Room {
    pub slots: u32,
    pub memory_mib: u32,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, TS,
)]
pub struct Generation {
    #[ts(type = "number")]
    pub boot: u64,
    #[ts(type = "number")]
    pub change: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct ReportedMachine {
    pub id: String,
    pub tenant: String,
    pub memory_mib: u32,
    pub state: MachineState,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct HostReport {
    pub url: String,
    pub pool: String,
    pub store: String,
    pub room: Room,
    pub egress_addresses: Vec<String>,
    pub generation: Generation,
    pub machines: Vec<ReportedMachine>,
    pub executions: Vec<ExecutionUsage>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, TS)]
pub struct HostReportReply {
    pub recorded_executions: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct HostInfo {
    pub url: String,
    pub pool: Option<String>,
    pub room: Room,
    #[ts(type = "number | null")]
    pub reported_ms_ago: Option<u64>,
    pub reporting: bool,
}
