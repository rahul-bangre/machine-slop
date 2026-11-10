use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct StartProcess {
    pub program: String,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cwd: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub stdin: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub keep_stdin_open: Option<bool>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub terminal: Option<TerminalSize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessStatus {
    Running,
    Finished { result: ProcessResult },
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessResult {
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
        output_complete: bool,
        stdin_complete: bool,
    },
    Unknown {
        message: String,
    },
}

#[derive(Debug, Serialize, TS)]
pub struct ProcessInfo {
    pub id: String,
    pub status: ProcessStatus,
    #[ts(type = "number")]
    pub first_seq: u64,
    #[ts(type = "number")]
    pub next_seq: u64,
    #[ts(type = "number")]
    pub stdin_offset: u64,
    pub stdin_open: bool,
    pub terminal: Option<TerminalSize>,
}

#[derive(Debug, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProcessEvent {
    Stdout {
        #[ts(type = "number")]
        seq: u64,
        data: String,
    },
    Stderr {
        #[ts(type = "number")]
        seq: u64,
        data: String,
    },
    Finished {
        #[ts(type = "number")]
        seq: u64,
        result: ProcessResult,
    },
    Gap {
        #[ts(type = "number")]
        first_seq: u64,
    },
}
