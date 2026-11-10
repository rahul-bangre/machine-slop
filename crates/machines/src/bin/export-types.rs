use std::{fs, path::Path};

use slop_machines::{
    CreateMachine, CreatePreview, CreateSnapshot, EgressCredential, HostInfo, MachineInfo,
    MachineState, PreviewInfo, Quotas, Room, SnapshotInfo, TenantUsage, UpdateMachine,
};
use ts_rs::TS;

fn main() -> std::io::Result<()> {
    let config = ts_rs::Config::default();
    let declarations = [
        CreateMachine::decl(&config),
        EgressCredential::decl(&config),
        UpdateMachine::decl(&config),
        MachineState::decl(&config),
        MachineInfo::decl(&config),
        CreateSnapshot::decl(&config),
        SnapshotInfo::decl(&config),
        CreatePreview::decl(&config),
        PreviewInfo::decl(&config),
        Quotas::decl(&config),
        TenantUsage::decl(&config),
        Room::decl(&config),
        HostInfo::decl(&config),
    ];
    let output = format!(
        "{}\n",
        declarations
            .map(|declaration| format!("export {declaration}"))
            .join("\n\n")
    );
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/sdk/src/machines.gen.ts");
    if std::env::args().nth(1).as_deref() == Some("--check") {
        if fs::read_to_string(path)? != output {
            return Err(std::io::Error::other(
                "machine types are stale; run make generate",
            ));
        }
        Ok(())
    } else {
        fs::write(path, output)
    }
}
