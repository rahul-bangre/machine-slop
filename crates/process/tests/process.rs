use std::time::Duration;

use slop_process::{ProcessError, ProcessResult, Processes, StartProcess};
use tokio::time::timeout;

fn sleep_request() -> StartProcess {
    StartProcess {
        program: "/bin/sleep".into(),
        args: Some(vec!["30".into()]),
        env: None,
        cwd: None,
        stdin: None,
    }
}

#[tokio::test]
async fn kill_before_supervisor_first_poll_is_not_lost() {
    let processes = Processes::default();
    let process = processes.start(sleep_request()).unwrap();
    process.kill();
    let result = timeout(Duration::from_secs(2), process.wait())
        .await
        .unwrap();
    assert!(matches!(
        result,
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
