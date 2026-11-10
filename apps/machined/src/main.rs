use std::{path::PathBuf, sync::Arc};

use slop_machines::Machines;
use tokio::net::TcpListener;

const CAPACITY: usize = 16;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    eprintln!("machined listening on http://{}", listener.local_addr()?);
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
            if let Err(error) = result { eprintln!("failed to listen for shutdown: {error}"); }
        }
        _ = terminate.recv() => {}
    }
    machines.shutdown().await;
}
