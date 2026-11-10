use slop_machines::Store;
use tokio::net::TcpListener;

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
    let (Some(address), Some(store), Some(state_dir), Some(tenants_file), None) = (
        args.next(),
        args.next(),
        args.next(),
        args.next(),
        args.next(),
    ) else {
        return Err(
            "usage: controld <listen address> <store url> <state dir> <tenants file>".into(),
        );
    };
    let key_file = std::env::var("SLOP_CONTROL_KEY_FILE")
        .map_err(|_| "SLOP_CONTROL_KEY_FILE names the file holding the control key")?;
    let control_key = std::fs::read_to_string(&key_file)?.trim().to_owned();
    let (tenants, keys) = slop_machines::parse_tenants(&std::fs::read_to_string(&tenants_file)?)?;
    let control = controld::Control::open(controld::Config {
        store: Store::open(&store)?,
        state_dir: state_dir.into(),
        tenants,
        keys,
        control_key,
    })
    .await?;
    tokio::spawn(control.clone().reconcile_periodically());
    let listener = TcpListener::bind(&address).await?;
    tracing::info!(address = %format!("http://{}", listener.local_addr()?), %store, "controld listening");
    axum::serve(listener, controld::app(control))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
