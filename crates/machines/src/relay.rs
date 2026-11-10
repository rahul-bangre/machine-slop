use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub async fn relay_until_either_side_ends(
    client: impl AsyncRead + AsyncWrite + Unpin,
    upstream: impl AsyncRead + AsyncWrite + Unpin,
) {
    let (mut from_client, mut to_client) = tokio::io::split(client);
    let (mut from_upstream, mut to_upstream) = tokio::io::split(upstream);
    tokio::select! {
        _ = tokio::io::copy(&mut from_client, &mut to_upstream) => {}
        _ = async {
            let _ = tokio::io::copy(&mut from_upstream, &mut to_client).await;
            let _ = to_client.shutdown().await;
        } => {}
    }
}
