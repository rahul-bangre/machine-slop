use std::{
    collections::{HashMap, VecDeque},
    io,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{Engine, prelude::BASE64_STANDARD};
use bytes::Bytes;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::watch,
    time::{Instant, sleep_until},
};
use uuid::Uuid;

use crate::{ProcessEvent, ProcessInfo, ProcessResult, ProcessStatus, StartProcess};

const MAX_PROCESSES: usize = 128;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_EVENTS: usize = 256;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum ProcessError {
    Capacity,
    ShuttingDown,
    Spawn(io::Error),
    InvalidCursor,
    Gap { first_seq: u64 },
}

#[derive(Default)]
pub struct Processes {
    registry: Mutex<Registry>,
}

#[derive(Default)]
struct Registry {
    entries: HashMap<String, Arc<Process>>,
    closed: bool,
}

impl Processes {
    pub fn start(&self, request: StartProcess) -> Result<Arc<Process>, ProcessError> {
        let mut registry = self.registry.lock().expect("process registry poisoned");
        if registry.closed {
            return Err(ProcessError::ShuttingDown);
        }
        let evict = if registry.entries.len() == MAX_PROCESSES {
            let finished = registry
                .entries
                .iter()
                .filter(|(_, process)| process.is_finished())
                .min_by_key(|(_, process)| process.created)
                .map(|(id, _)| id.clone());
            match finished {
                Some(id) => Some(id),
                None => return Err(ProcessError::Capacity),
            }
        } else {
            None
        };
        let mut command = Command::new(request.program);
        command
            .args(request.args.unwrap_or_default())
            .envs(request.env.unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = request.cwd {
            command.current_dir(cwd);
        }
        let child = command.spawn().map_err(ProcessError::Spawn)?;
        let process = Arc::new(Process::new());
        if let Some(id) = evict {
            registry.entries.remove(&id);
        }
        registry.entries.insert(process.id.clone(), process.clone());
        tokio::spawn(supervise(process.clone(), child, request.stdin));
        Ok(process)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Process>> {
        self.registry
            .lock()
            .expect("process registry poisoned")
            .entries
            .get(id)
            .cloned()
    }

    pub async fn shutdown(&self) {
        let processes: Vec<_> = {
            let mut registry = self.registry.lock().expect("process registry poisoned");
            registry.closed = true;
            registry.entries.values().cloned().collect()
        };
        for process in &processes {
            process.kill();
        }
        for process in processes {
            process.wait().await;
        }
    }
}

pub struct Process {
    id: String,
    created: Instant,
    log: Mutex<Log>,
    changed: watch::Sender<()>,
    kill: watch::Sender<bool>,
}

struct Log {
    status: ProcessStatus,
    events: VecDeque<(u64, Bytes)>,
    bytes: usize,
    next_seq: u64,
}

pub struct EventBatch {
    pub chunks: Vec<Bytes>,
    pub next_seq: u64,
    pub finished: bool,
}

impl Process {
    fn new() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            created: Instant::now(),
            log: Mutex::new(Log {
                status: ProcessStatus::Running,
                events: VecDeque::new(),
                bytes: 0,
                next_seq: 0,
            }),
            changed: watch::channel(()).0,
            kill: watch::channel(false).0,
        }
    }

    pub fn info(&self) -> ProcessInfo {
        let log = self.log.lock().expect("process log poisoned");
        ProcessInfo {
            id: self.id.clone(),
            status: log.status.clone(),
            first_seq: log.first_seq(),
            next_seq: log.next_seq,
        }
    }

    pub async fn wait(&self) -> ProcessResult {
        let mut changed = self.subscribe();
        loop {
            if let ProcessStatus::Finished { result } = self.info().status {
                return result;
            }
            changed
                .changed()
                .await
                .expect("process owns notification sender");
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }

    pub fn kill(&self) {
        self.kill.send_replace(true);
    }

    pub fn events(&self, cursor: u64) -> Result<EventBatch, ProcessError> {
        let log = self.log.lock().expect("process log poisoned");
        if cursor > log.next_seq {
            return Err(ProcessError::InvalidCursor);
        }
        if cursor < log.first_seq() {
            return Err(ProcessError::Gap {
                first_seq: log.first_seq(),
            });
        }
        Ok(EventBatch {
            chunks: log
                .events
                .iter()
                .filter(|(seq, _)| *seq >= cursor)
                .map(|(_, bytes)| bytes.clone())
                .collect(),
            next_seq: log.next_seq,
            finished: matches!(log.status, ProcessStatus::Finished { .. }),
        })
    }

    fn is_finished(&self) -> bool {
        matches!(
            self.log.lock().expect("process log poisoned").status,
            ProcessStatus::Finished { .. }
        )
    }

    fn output(&self, stderr: bool, bytes: &[u8]) {
        let mut log = self.log.lock().expect("process log poisoned");
        let data = BASE64_STANDARD.encode(bytes);
        let event = if stderr {
            ProcessEvent::Stderr {
                seq: log.next_seq,
                data,
            }
        } else {
            ProcessEvent::Stdout {
                seq: log.next_seq,
                data,
            }
        };
        log.append(event);
        self.changed.send_replace(());
    }

    fn finish(&self, result: ProcessResult) {
        let mut log = self.log.lock().expect("process log poisoned");
        log.status = ProcessStatus::Finished {
            result: result.clone(),
        };
        let event = ProcessEvent::Finished {
            seq: log.next_seq,
            result,
        };
        log.append(event);
        self.changed.send_replace(());
    }
}

impl Log {
    fn first_seq(&self) -> u64 {
        self.events.front().map_or(self.next_seq, |(seq, _)| *seq)
    }

    fn append(&mut self, event: ProcessEvent) {
        let mut bytes = serde_json::to_vec(&event).expect("process event is serializable");
        bytes.push(b'\n');
        self.bytes += bytes.len();
        self.events.push_back((self.next_seq, Bytes::from(bytes)));
        self.next_seq += 1;
        while self.bytes > MAX_OUTPUT_BYTES || self.events.len() > MAX_EVENTS {
            self.bytes -= self
                .events
                .pop_front()
                .expect("output buffer is nonempty")
                .1
                .len();
        }
    }
}

async fn supervise(process: Arc<Process>, mut child: Child, input: Option<String>) {
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let mut stdin = child.stdin.take();
    let input = input.unwrap_or_default();
    let mut input_offset = 0;
    let mut kill = process.kill.subscribe();
    let mut out = [0; 4096];
    let mut err = [0; 4096];
    let mut out_open = true;
    let mut err_open = true;
    let mut output_complete = true;
    let mut status = None;
    let mut kill_requested = false;
    let mut drain_deadline = Instant::now();
    let result = loop {
        if input_offset == input.len() || status.is_some() {
            stdin.take();
        }
        if let Some(exit) = status.take() {
            if !out_open && !err_open {
                break exit_result(exit, output_complete, input_offset == input.len());
            }
            status = Some(exit);
        }
        tokio::select! {
            result = child.wait(), if status.is_none() => {
                match result {
                    Ok(exit) => {
                        status = Some(exit);
                        drain_deadline = Instant::now() + DRAIN_TIMEOUT;
                    }
                    Err(error) => break ProcessResult::Unknown { message: error.to_string() },
                }
            }
            requested = kill.wait_for(|requested| *requested), if status.is_none() && !kill_requested => {
                if requested.is_ok() {
                    kill_requested = true;
                    if let Err(error) = child.start_kill() {
                        break ProcessResult::Unknown { message: error.to_string() };
                    }
                }
            }
            write = async { stdin.as_mut().expect("stdin is open").write(&input.as_bytes()[input_offset..]).await }, if stdin.is_some() => {
                match write {
                    Ok(0) | Err(_) => { stdin.take(); }
                    Ok(count) => input_offset += count,
                }
            }
            read = stdout.read(&mut out), if out_open => {
                match read {
                    Ok(0) => out_open = false,
                    Ok(count) => process.output(false, &out[..count]),
                    Err(_) => { out_open = false; output_complete = false; }
                }
            }
            read = stderr.read(&mut err), if err_open => {
                match read {
                    Ok(0) => err_open = false,
                    Ok(count) => process.output(true, &err[..count]),
                    Err(_) => { err_open = false; output_complete = false; }
                }
            }
            () = sleep_until(drain_deadline), if status.is_some() => {
                break exit_result(status.take().expect("child exited"), false, input_offset == input.len());
            }
        }
    };
    process.finish(result);
}

#[cfg(unix)]
fn exit_result(
    status: std::process::ExitStatus,
    output_complete: bool,
    stdin_complete: bool,
) -> ProcessResult {
    use std::os::unix::process::ExitStatusExt;
    ProcessResult::Exited {
        code: status.code(),
        signal: status.signal(),
        output_complete,
        stdin_complete,
    }
}

#[cfg(not(unix))]
fn exit_result(
    status: std::process::ExitStatus,
    output_complete: bool,
    stdin_complete: bool,
) -> ProcessResult {
    ProcessResult::Exited {
        code: status.code(),
        signal: None,
        output_complete,
        stdin_complete,
    }
}
