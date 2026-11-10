use std::{
    collections::{HashMap, VecDeque},
    io,
    os::unix::process::ExitStatusExt,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{Engine, prelude::BASE64_STANDARD};
use bytes::Bytes;
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::{mpsc, oneshot, watch},
    time::{Instant, sleep_until},
};
use uuid::Uuid;

use crate::{ProcessEvent, ProcessInfo, ProcessResult, ProcessStatus, StartProcess};

const MAX_PROCESSES: usize = 128;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_EVENTS: usize = 256;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const STDIN_QUEUE: usize = 8;

#[derive(Debug)]
pub enum ProcessError {
    Capacity,
    ShuttingDown,
    Spawn(io::Error),
    InvalidCursor,
    Gap { first_seq: u64 },
    StdinClosed { offset: u64 },
    StdinOffsetAhead { offset: u64 },
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
            .process_group(0)
            .kill_on_drop(true);
        if let Some(cwd) = request.cwd {
            command.current_dir(cwd);
        }
        let child = command.spawn().map_err(ProcessError::Spawn)?;
        let group = child
            .id()
            .and_then(|id| Pid::from_raw(id as i32))
            .expect("unreaped child has a pid");
        let (stdin_requests, requests) = mpsc::channel(STDIN_QUEUE);
        let process = Arc::new(Process::new(stdin_requests));
        let initial_stdin = StdinWrite {
            offset: 0,
            data: Bytes::from(request.stdin.unwrap_or_default()),
            close: !request.keep_stdin_open.unwrap_or(false),
            reply: None,
        };
        if let Some(id) = evict {
            registry.entries.remove(&id);
        }
        registry.entries.insert(process.id.clone(), process.clone());
        tokio::spawn(supervise(
            process.clone(),
            child,
            group,
            initial_stdin,
            requests,
        ));
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
    stdin: mpsc::Sender<StdinWrite>,
}

struct Log {
    status: ProcessStatus,
    events: VecDeque<(u64, Bytes)>,
    bytes: usize,
    next_seq: u64,
    stdin_offset: u64,
    stdin_open: bool,
}

struct StdinWrite {
    offset: u64,
    data: Bytes,
    close: bool,
    reply: Option<oneshot::Sender<Result<(), ProcessError>>>,
}

impl StdinWrite {
    fn admit(self, open: bool, offset: u64) -> Option<Self> {
        if !open {
            self.reply(Err(ProcessError::StdinClosed { offset }));
            return None;
        }
        if self.offset > offset {
            self.reply(Err(ProcessError::StdinOffsetAhead { offset }));
            return None;
        }
        let already_written = (offset - self.offset).min(self.data.len() as u64) as usize;
        Some(Self {
            data: self.data.slice(already_written..),
            ..self
        })
    }

    fn reply(self, outcome: Result<(), ProcessError>) {
        if let Some(reply) = self.reply {
            let _ = reply.send(outcome);
        }
    }
}

pub struct EventBatch {
    pub chunks: Vec<Bytes>,
    pub next_seq: u64,
    pub finished: bool,
}

impl Process {
    fn new(stdin: mpsc::Sender<StdinWrite>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            created: Instant::now(),
            log: Mutex::new(Log {
                status: ProcessStatus::Running,
                events: VecDeque::new(),
                bytes: 0,
                next_seq: 0,
                stdin_offset: 0,
                stdin_open: true,
            }),
            changed: watch::channel(()).0,
            kill: watch::channel(false).0,
            stdin,
        }
    }

    pub fn info(&self) -> ProcessInfo {
        let log = self.log.lock().expect("process log poisoned");
        ProcessInfo {
            id: self.id.clone(),
            status: log.status.clone(),
            first_seq: log.first_seq(),
            next_seq: log.next_seq,
            stdin_offset: log.stdin_offset,
            stdin_open: log.stdin_open,
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

    pub async fn write_stdin(
        &self,
        offset: u64,
        data: Bytes,
        close: bool,
    ) -> Result<(), ProcessError> {
        let (reply, outcome) = oneshot::channel();
        let request = StdinWrite {
            offset,
            data,
            close,
            reply: Some(reply),
        };
        let closed = || ProcessError::StdinClosed {
            offset: self.info().stdin_offset,
        };
        if self.stdin.send(request).await.is_err() {
            return Err(closed());
        }
        outcome.await.unwrap_or_else(|_| Err(closed()))
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

    fn record_stdin(&self, offset: u64, open: bool) {
        let mut log = self.log.lock().expect("process log poisoned");
        log.stdin_offset = offset;
        log.stdin_open = open;
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

fn end_process_group(group: Pid) {
    let _ = kill_process_group(group, Signal::KILL);
}

async fn supervise(
    process: Arc<Process>,
    mut child: Child,
    group: Pid,
    initial_stdin: StdinWrite,
    mut stdin_requests: mpsc::Receiver<StdinWrite>,
) {
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let mut stdin = child.stdin.take();
    let mut stdin_offset = 0;
    let mut stdin_complete = true;
    let mut pending = initial_stdin.admit(true, 0);
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
        if status.is_some() {
            stdin.take();
            if let Some(unwritten) = pending.take() {
                stdin_complete = false;
                unwritten.reply(Err(ProcessError::StdinClosed {
                    offset: stdin_offset,
                }));
            }
        }
        if let Some(written) = pending.take_if(|pending| pending.data.is_empty()) {
            if written.close {
                stdin.take();
            }
            written.reply(Ok(()));
        }
        process.record_stdin(stdin_offset, stdin.is_some());
        if let Some(exit) = status.take() {
            if !out_open && !err_open {
                break exit_result(exit, output_complete, stdin_complete);
            }
            status = Some(exit);
        }
        tokio::select! {
            result = child.wait(), if status.is_none() => {
                match result {
                    Ok(exit) => {
                        end_process_group(group);
                        status = Some(exit);
                        drain_deadline = Instant::now() + DRAIN_TIMEOUT;
                    }
                    Err(error) => break ProcessResult::Unknown { message: error.to_string() },
                }
            }
            requested = kill.wait_for(|requested| *requested), if status.is_none() && !kill_requested => {
                if requested.is_ok() {
                    kill_requested = true;
                    if let Err(error) = kill_process_group(group, Signal::KILL) {
                        break ProcessResult::Unknown { message: error.to_string() };
                    }
                }
            }
            request = stdin_requests.recv(), if pending.is_none() => {
                if let Some(request) = request {
                    pending = request.admit(stdin.is_some(), stdin_offset);
                }
            }
            write = async {
                stdin.as_mut().expect("stdin is open").write(&pending.as_ref().expect("stdin write is pending").data).await
            }, if stdin.is_some() && pending.is_some() => {
                match write {
                    Ok(count) if count > 0 => {
                        let written = pending.as_mut().expect("stdin write is pending");
                        written.data = written.data.slice(count..);
                        stdin_offset += count as u64;
                    }
                    _ => {
                        stdin.take();
                        stdin_complete = false;
                        if let Some(unwritten) = pending.take() {
                            unwritten.reply(Err(ProcessError::StdinClosed { offset: stdin_offset }));
                        }
                    }
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
                break exit_result(status.take().expect("child exited"), false, stdin_complete);
            }
        }
    };
    process.record_stdin(stdin_offset, false);
    process.finish(result);
}

fn exit_result(status: ExitStatus, output_complete: bool, stdin_complete: bool) -> ProcessResult {
    ProcessResult::Exited {
        code: status.code(),
        signal: status.signal(),
        output_complete,
        stdin_complete,
    }
}
