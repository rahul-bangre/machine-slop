use std::{path::PathBuf, sync::Arc};

use slop_machines::{Limits, Machines, Network, PreviewOrigin, default_memory_budget_mib};
use tokio::net::TcpListener;

const MAX_RUNNING_VMS: usize = 16;
const DEFAULT_SPARE_MACHINES: usize = 2;

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
    let (Some(address), Some(image_dir), Some(state_dir), None) =
        (args.next(), args.next(), args.next(), args.next())
    else {
        return Err("usage: machined <listen address> <image dir> <state dir>".into());
    };
    let default_budget = default_memory_budget_mib()?;
    let limits = Limits {
        running_vms: MAX_RUNNING_VMS,
        memory_mib: env_number("SLOP_MEMORY_MIB")?
            .map_or(default_budget, |budget: u32| budget.min(default_budget)),
        spare_machines: env_number("SLOP_SPARE_MACHINES")?.unwrap_or(DEFAULT_SPARE_MACHINES),
    };
    tracing::info!(
        running_vms = limits.running_vms,
        memory_mib = limits.memory_mib,
        spare_machines = limits.spare_machines,
        "admission limits"
    );
    let (previews, preview_listener) = match (
        env_text("SLOP_PREVIEW_URL")?,
        env_text("SLOP_PREVIEW_LISTEN")?,
    ) {
        (Some(url), Some(listen)) => (
            Some(PreviewOrigin::parse(&url)?),
            Some(TcpListener::bind(&listen).await?),
        ),
        (None, None) => (None, None),
        _ => return Err("set SLOP_PREVIEW_URL and SLOP_PREVIEW_LISTEN together".into()),
    };
    let egress_addresses = env_text("SLOP_EGRESS_ADDRESSES")?
        .unwrap_or_default()
        .split(',')
        .filter(|address| !address.trim().is_empty())
        .map(|address| {
            address
                .trim()
                .parse()
                .map_err(|_| format!("SLOP_EGRESS_ADDRESSES has an invalid address {address:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let network = Network {
        egress_addresses,
        previews,
    };
    let machines = Machines::open(
        PathBuf::from(image_dir),
        PathBuf::from(state_dir),
        limits,
        network,
    )
    .await?;
    let listener = TcpListener::bind(&address).await?;
    let app = machined::app(machines.clone());
    if let Some(preview_listener) = preview_listener {
        tracing::info!(address = %format!("http://{}", preview_listener.local_addr()?), "previews listening");
        let previews = machined::preview_app(machines.clone());
        tokio::spawn(async move {
            if let Err(error) = axum::serve(preview_listener, previews).await {
                tracing::error!(%error, "preview server failed");
            }
        });
    }

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

fn env_text(name: &str) -> Result<Option<String>, String> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(format!("{name}: {error}")),
    }
}

fn env_number<T: std::str::FromStr>(name: &str) -> Result<Option<T>, String> {
    env_text(name)?
        .map(|value| {
            value
                .parse()
                .map_err(|_| format!("{name} must be a whole number, not {value:?}"))
        })
        .transpose()
}
