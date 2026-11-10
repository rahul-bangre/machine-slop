use std::{path::PathBuf, sync::Arc};

use slop_machines::Machines;
use tokio::net::TcpListener;

const CAPACITY: usize = 16;

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
    let machines = Arc::new(
        Machines::open(PathBuf::from(image_dir), PathBuf::from(state_dir), CAPACITY).await?,
    );
    let listener = TcpListener::bind(&address).await?;
    let app = machined::app(machines.clone());

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
