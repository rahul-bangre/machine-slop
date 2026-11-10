use std::sync::Arc;

use slop_process::Processes;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:3000".into());
    let listener = TcpListener::bind(&address).await?;
    let processes = Arc::new(Processes::default());
    let app = agentd::app(processes.clone());

    eprintln!("agentd listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(processes))
        .await?;
    Ok(())
}

async fn shutdown(processes: Arc<Processes>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("register SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result { eprintln!("failed to listen for shutdown: {error}"); }
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("failed to listen for shutdown: {error}");
    }
    processes.shutdown().await;
}
