use std::{fs, path::Path};

use slop_process::{
    ProcessEvent, ProcessInfo, ProcessResult, ProcessStatus, StartProcess, TerminalSize,
};
use ts_rs::TS;

fn main() -> std::io::Result<()> {
    let config = ts_rs::Config::default();
    let declarations = [
        TerminalSize::decl(&config),
        StartProcess::decl(&config),
        ProcessResult::decl(&config),
        ProcessStatus::decl(&config),
        ProcessInfo::decl(&config),
        ProcessEvent::decl(&config),
    ];
    let output = format!(
        "{}\n",
        declarations
            .map(|declaration| format!("export {declaration}"))
            .join("\n\n")
    );
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/sdk/src/process.gen.ts");
    if std::env::args().nth(1).as_deref() == Some("--check") {
        if fs::read_to_string(path)? != output {
            return Err(std::io::Error::other(
                "process types are stale; run make generate",
            ));
        }
        Ok(())
    } else {
        fs::write(path, output)
    }
}
