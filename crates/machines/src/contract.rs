use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Deserialize, TS)]
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
    #[serde(default = "every_host")]
    #[ts(as = "Option<Vec<String>>", optional)]
    pub allowed_hosts: Vec<String>,
}

fn every_host() -> Vec<String> {
    vec!["*".to_owned()]
}

#[derive(Debug, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct UpdateMachine {
    pub allowed_hosts: Vec<String>,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MachineState {
    Running,
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
