use std::sync::Arc;

use axum::Router;
use slop_process::Processes;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:3000".into());
    let processes = Arc::new(Processes::default());
    let app = agentd::app(processes.clone());

    match address.strip_prefix("vsock:") {
        Some(port) => serve_vsock(port.parse()?, app, processes).await,
        None => {
            let listener = TcpListener::bind(&address).await?;
            eprintln!("agentd listening on http://{}", listener.local_addr()?);
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown(processes))
                .await?;
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
async fn serve_vsock(
    port: u32,
    app: Router,
    processes: Arc<Processes>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = vsock::Listener::bind(port)?;
    eprintln!("agentd listening on vsock port {port}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(processes))
        .await?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn serve_vsock(
    _port: u32,
    _app: Router,
    _processes: Arc<Processes>,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("vsock listening requires Linux".into())
}

#[cfg(target_os = "linux")]
mod vsock {
    use std::{io, time::Duration};

    use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener, VsockStream};

    pub struct Listener(VsockListener);

    impl Listener {
        pub fn bind(port: u32) -> io::Result<Self> {
            VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port)).map(Self)
        }
    }

    impl axum::serve::Listener for Listener {
        type Io = VsockStream;
        type Addr = VsockAddr;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            loop {
                match self.0.accept().await {
                    Ok(connection) => return connection,
                    Err(error) => {
                        eprintln!("vsock accept failed: {error}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }

        fn local_addr(&self) -> io::Result<Self::Addr> {
            self.0.local_addr()
        }
    }
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
