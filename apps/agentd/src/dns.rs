use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, UdpSocket},
    sync::Semaphore,
};
use tokio_vsock::{VsockAddr, VsockStream};

const HOST_CID: u32 = 2;
const MAX_DATAGRAM_BYTES: usize = 4096;
const MAX_QUERIES_IN_FLIGHT: usize = 64;
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const ACCEPT_RETRY: Duration = Duration::from_millis(50);

pub struct Relay {
    udp: Arc<UdpSocket>,
    tcp: TcpListener,
    host: VsockAddr,
}

impl Relay {
    pub async fn bind(address: SocketAddr, host_port: u32) -> io::Result<Self> {
        Ok(Self {
            udp: Arc::new(UdpSocket::bind(address).await?),
            tcp: TcpListener::bind(address).await?,
            host: VsockAddr::new(HOST_CID, host_port),
        })
    }

    pub async fn run(self) {
        let in_flight = Arc::new(Semaphore::new(MAX_QUERIES_IN_FLIGHT));
        let mut datagram = vec![0; MAX_DATAGRAM_BYTES];
        loop {
            tokio::select! {
                received = self.udp.recv_from(&mut datagram) => {
                    let Ok((length, client)) = received else {
                        tokio::time::sleep(ACCEPT_RETRY).await;
                        continue;
                    };
                    let Ok(permit) = in_flight.clone().try_acquire_owned() else { continue };
                    let (udp, host, query) = (self.udp.clone(), self.host, datagram[..length].to_vec());
                    tokio::spawn(async move {
                        if let Ok(Ok(response)) = tokio::time::timeout(QUERY_TIMEOUT, ask(host, &query)).await {
                            let _ = udp.send_to(&response, client).await;
                        }
                        drop(permit);
                    });
                }
                accepted = self.tcp.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        tokio::time::sleep(ACCEPT_RETRY).await;
                        continue;
                    };
                    let Ok(permit) = in_flight.clone().try_acquire_owned() else { continue };
                    let host = self.host;
                    tokio::spawn(async move {
                        if let Ok(mut upstream) = VsockStream::connect(host).await {
                            let _ = copy_bidirectional(&mut client, &mut upstream).await;
                        }
                        drop(permit);
                    });
                }
            }
        }
    }
}

async fn ask(host: VsockAddr, query: &[u8]) -> io::Result<Vec<u8>> {
    let length = u16::try_from(query.len()).map_err(io::Error::other)?;
    let mut upstream = VsockStream::connect(host).await?;
    upstream.write_all(&length.to_be_bytes()).await?;
    upstream.write_all(query).await?;
    let mut length = [0; 2];
    upstream.read_exact(&mut length).await?;
    let mut response = vec![0; usize::from(u16::from_be_bytes(length))];
    upstream.read_exact(&mut response).await?;
    Ok(response)
}
