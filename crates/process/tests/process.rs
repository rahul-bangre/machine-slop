use std::{path::Path, time::Duration};

use base64::{Engine, prelude::BASE64_STANDARD};
use bytes::Bytes;
use rustix::process::{Pid, test_kill_process};
use slop_process::{Process, ProcessError, ProcessResult, Processes, StartProcess};
use tokio::time::{Instant, sleep, timeout};

fn command(program: &str, args: &[&str]) -> StartProcess {
    StartProcess {
        program: program.into(),
        args: Some(args.iter().map(|arg| arg.to_string()).collect()),
        env: None,
        cwd: None,
        stdin: None,
        keep_stdin_open: None,
    }
}

fn sleep_request() -> StartProcess {
    command("/bin/sleep", &["30"])
}

async fn finish(process: &Process) -> ProcessResult {
    timeout(Duration::from_secs(5), process.wait())
        .await
        .expect("process finishes")
}

fn stdout(process: &Process) -> String {
    let batch = process.events(0).unwrap();
    let mut text = Vec::new();
    for chunk in batch.chunks {
        let event: serde_json::Value = serde_json::from_slice(&chunk).unwrap();
        if event["type"] == "stdout" {
            text.extend(
                BASE64_STANDARD
                    .decode(event["data"].as_str().unwrap())
                    .unwrap(),
            );
        }
    }
    String::from_utf8(text).unwrap()
}

async fn read_pid(path: &Path) -> Pid {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(raw) = text.trim().parse()
        {
            return Pid::from_raw(raw).unwrap();
        }
        assert!(Instant::now() < deadline, "descendant pid was not written");
        sleep(Duration::from_millis(10)).await;
    }
}

async fn assert_gone(pid: Pid) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while test_kill_process(pid).is_ok() {
        assert!(Instant::now() < deadline, "descendant {pid:?} survived");
        sleep(Duration::from_millis(10)).await;
    }
}

fn pid_file(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("slop-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

#[tokio::test]
async fn kill_before_supervisor_first_poll_is_not_lost() {
    let processes = Processes::default();
    let process = processes.start(sleep_request()).unwrap();
    process.kill();
    assert!(matches!(
        finish(&process).await,
        ProcessResult::Exited {
            signal: Some(9),
            ..
        }
    ));
}

#[tokio::test]
async fn shutdown_closes_admission_and_waits_for_owned_children() {
    let processes = Processes::default();
    let process = processes.start(sleep_request()).unwrap();
    timeout(Duration::from_secs(2), processes.shutdown())
        .await
        .unwrap();
    assert!(matches!(
        processes.start(sleep_request()),
        Err(ProcessError::ShuttingDown)
    ));
    assert!(matches!(
        process.wait().await,
        ProcessResult::Exited {
            signal: Some(9),
            ..
        }
    ));
}

#[tokio::test]
async fn kill_ends_every_process_in_the_group() {
    let path = pid_file("kill");
    let processes = Processes::default();
    let process = processes
        .start(command(
            "/bin/sh",
            &[
                "-c",
                "sleep 30 & echo $! > \"$1\"; wait",
                "sh",
                path.to_str().unwrap(),
            ],
        ))
        .unwrap();
    let descendant = read_pid(&path).await;
    process.kill();
    assert!(matches!(
        finish(&process).await,
        ProcessResult::Exited {
            signal: Some(9),
            output_complete: true,
            ..
        }
    ));
    assert_gone(descendant).await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn exit_ends_descendants_left_in_the_group() {
    let path = pid_file("exit");
    let processes = Processes::default();
    let process = processes
        .start(command(
            "/bin/sh",
            &[
                "-c",
                "sleep 30 & echo $! > \"$1\"",
                "sh",
                path.to_str().unwrap(),
            ],
        ))
        .unwrap();
    assert!(matches!(
        finish(&process).await,
        ProcessResult::Exited {
            code: Some(0),
            output_complete: true,
            ..
        }
    ));
    assert_gone(read_pid(&path).await).await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn stdin_writes_are_ordered_by_offset_and_retries_write_once() {
    let processes = Processes::default();
    let process = processes
        .start(StartProcess {
            keep_stdin_open: Some(true),
            ..command("/bin/cat", &[])
        })
        .unwrap();
    let write = |offset, data: &'static str, close| {
        process.write_stdin(offset, Bytes::from_static(data.as_bytes()), close)
    };
    write(0, "ab", false).await.unwrap();
    write(0, "ab", false).await.unwrap();
    write(1, "bc", false).await.unwrap();
    assert!(matches!(
        write(10, "x", false).await,
        Err(ProcessError::StdinOffsetAhead { offset: 3 })
    ));
    let info = process.info();
    assert_eq!((info.stdin_offset, info.stdin_open), (3, true));
    write(3, "", true).await.unwrap();
    assert!(matches!(
        finish(&process).await,
        ProcessResult::Exited {
            code: Some(0),
            stdin_complete: true,
            ..
        }
    ));
    assert_eq!(stdout(&process), "abc");
    assert!(matches!(
        write(3, "d", false).await,
        Err(ProcessError::StdinClosed { offset: 3 })
    ));
    let info = process.info();
    assert_eq!((info.stdin_offset, info.stdin_open), (3, false));
}

#[tokio::test]
async fn input_the_program_never_reads_is_reported_incomplete() {
    let processes = Processes::default();
    let process = processes
        .start(StartProcess {
            stdin: Some("x".repeat(4 * 1024 * 1024)),
            ..command("/usr/bin/true", &[])
        })
        .unwrap();
    assert!(matches!(
        finish(&process).await,
        ProcessResult::Exited {
            code: Some(0),
            stdin_complete: false,
            ..
        }
    ));
}
