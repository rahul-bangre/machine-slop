use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Default, Deserialize, TS)]
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
    #[serde(default)]
    #[ts(optional = nullable)]
    pub snapshot_id: Option<String>,
}

fn listed<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    Vec::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct UpdateMachine {
    pub allowed_hosts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MachineState {
    Running,
    Paused,
    Exited { message: String },
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct MachineInfo {
    pub id: String,
    pub state: MachineState,
    pub vcpus: u8,
    pub memory_mib: u32,
    #[ts(type = "number")]
    pub boot_ms: u64,
    pub idle_timeout_secs: u32,
    pub allowed_hosts: Vec<String>,
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
}
