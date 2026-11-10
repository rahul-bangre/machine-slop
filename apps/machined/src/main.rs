use std::{path::PathBuf, sync::Arc};

use slop_machines::{Limits, Machines, default_memory_budget_mib};
use tokio::net::TcpListener;

const MAX_RUNNING_VMS: usize = 16;
const DEFAULT_SPARE_MACHINES: usize = 2;
const DEFAULT_FIRST_JAIL_UID: u32 = 1_000_000_000;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .json()
        .with_writer(std::io::stderr)
        .init();
    let mut args = std::env::args().skip(1);
    let (Some(address), Some(image_dir), Some(state_dir), Some(tenants_file), None) = (
        args.next(),
        args.next(),
        args.next(),
        args.next(),
        args.next(),
    ) else {
        return Err(
            "usage: machined <listen address> <image dir> <state dir> <tenants file>".into(),
        );
    };
    let (tenants, keys) = machined::parse_tenants(&std::fs::read_to_string(&tenants_file)?)?;
    let default_budget = default_memory_budget_mib()?;
    let limits = Limits {
        running_vms: MAX_RUNNING_VMS,
        memory_mib: env_number("SLOP_MEMORY_MIB")?
            .map_or(default_budget, |budget: u32| budget.min(default_budget)),
        spare_machines: env_number("SLOP_SPARE_MACHINES")?.unwrap_or(DEFAULT_SPARE_MACHINES),
        first_jail_uid: env_number("SLOP_FIRST_JAIL_UID")?.unwrap_or(DEFAULT_FIRST_JAIL_UID),
    };
    tracing::info!(
        running_vms = limits.running_vms,
        memory_mib = limits.memory_mib,
        spare_machines = limits.spare_machines,
        first_jail_uid = limits.first_jail_uid,
        "admission limits"
    );
    tracing::info!(tenants = tenants.len(), "tenants configured");
    let machines = Machines::open(
        PathBuf::from(image_dir),
        PathBuf::from(state_dir),
        limits,
        tenants,
    )
    .await?;
    let listener = TcpListener::bind(&address).await?;
    let app = machined::app(Arc::new(machined::Host {
        machines: machines.clone(),
        keys,
    }));

    tracing::info!(address = %format!("http://{}", listener.local_addr()?), "machined listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(machines))
        .await?;
    Ok(())
}

async fn shutdown(machines: Arc<Machines>) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).expect("register SIGTERM handler");
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result { tracing::error!(%error, "failed to listen for shutdown"); }
        }
        _ = terminate.recv() => {}
    }
    tracing::info!("shutdown started");
    machines.shutdown().await;
    tracing::info!("shutdown completed");
}

fn env_number<T: std::str::FromStr>(name: &str) -> Result<Option<T>, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map(Some)
            .map_err(|_| format!("{name} must be a whole number, not {value:?}")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(format!("{name}: {error}")),
    }
}
