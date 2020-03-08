use std::env;
use std::io::ErrorKind;
use std::time::Duration;

use async_std::io::prelude::*;
use async_std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use async_std::task;
use log::{debug, error, info, trace};
use once_cell::sync::OnceCell;

use anyhow::anyhow;
use sagittarius::{Channel, Relay, DEFAULT_SERVER_ADDR};

static PORT: OnceCell<u16> = OnceCell::new();
static REMOTE: OnceCell<String> = OnceCell::new();

fn main() -> Result<(), anyhow::Error> {
    sagittarius::init_log();

    let port = env::args()
        .nth(1)
        .unwrap_or_else(|| "8888".to_owned())
        .parse::<u16>()?;
    PORT.set(port).unwrap();
    info!("Listening on: {}", port);

    let remote = env::args()
        .nth(2)
        .unwrap_or_else(|| DEFAULT_SERVER_ADDR.to_owned());
    REMOTE.set(remote.clone()).unwrap();
    info!("Proxying to: {}", remote);

    task::block_on(accept_loop(("127.0.0.1", port)))
}

async fn accept_loop(addr: impl ToSocketAddrs) -> Result<(), anyhow::Error> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                trace!("accept request from {}", addr);
                socket.set_nodelay(true)?;

                task::spawn(async move {
                    let mut session = Session {
                        inbound: socket,
                        peer_addr: addr,
                        buf: vec![],
                        host: vec![],
                        port: [0; 2],
                        outbound: None,
                    };
                    if let Err(e) = session.run().await {
                        error!("Error: {:?}", e);
                    }
                });
            }
            Err(e) => {
                error!("Accept error: {:?}. Sleeping 0.5s.", e);
                task::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

#[derive(Debug)]
struct Session {
    inbound: TcpStream,
    peer_addr: SocketAddr,
    buf: Vec<u8>,

    host: Vec<u8>,
    port: [u8; 2],
    outbound: Option<TcpStream>,
}

const METHOD_SELECTION_RESP: [u8; 2] = [0x05, 0x00];
const SOCKS_RESP_PREFIX: [u8; 4] = [0x05, 0x00, 0x00, 0x01];

impl Session {
    async fn run(&mut self) -> Result<(), anyhow::Error> {
        self.method_selection().await?;
        self.socks().await?;
        self.address().await?;
        self.relay().await
    }

    async fn method_selection(&mut self) -> Result<(), anyhow::Error> {
        let mut buf = [0 as u8; 2];
        self.inbound.read_exact(&mut buf).await?;

        validate_version(&buf)?;

        let n_methods = buf[1];
        self.buf.resize(n_methods as usize, 0);

        self.inbound.read_exact(&mut self.buf).await?;
        self.inbound.write_all(&METHOD_SELECTION_RESP).await?;
        Ok(())
    }

    async fn socks(&mut self) -> Result<(), anyhow::Error> {
        let mut buf = [0 as u8; 4];
        self.inbound.read_exact(&mut buf).await?;

        validate_version(&buf)?;

        if buf[1] != 0x01 {
            self.inbound
                .write_all(&[0x05, 0x07, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
                .await?;
            return Err(anyhow!("unsupported CMD {:#04x}", buf[1]));
        }

        if buf[2] != 0x0 {
            return Err(anyhow!("wrong RSV {:#04x}", buf[2]));
        }

        match buf[3] {
            0x01 => {
                self.inbound
                    .write_all(&[0x05, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
                    .await?;
                return Err(anyhow!("address type IPV4 not supported"));
            }
            0x03 => {
                let mut buf = [0 as u8; 1];
                self.inbound.read_exact(&mut buf).await?;
                let addr_len = buf[0] as usize;

                self.buf.resize(addr_len, 0);
                self.inbound.read_exact(&mut self.buf).await?;
                self.host.extend(&self.buf);

                self.inbound.read_exact(&mut self.port).await?;

                trace!(
                    "accept request to {}:{}",
                    self.normalized_host(),
                    self.normalized_port()
                );
            }
            0x04 => {
                self.inbound
                    .write_all(&[0x05, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
                    .await?;
                return Err(anyhow!("address type IPV6 not supported"));
            }

            _ => {
                self.inbound
                    .write_all(&[0x05, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
                    .await?;
                return Err(anyhow!("unknown address type {:#04x}", buf[3]));
            }
        };

        Ok(())
    }

    async fn address(&mut self) -> Result<(), anyhow::Error> {
        let remote = match TcpStream::connect(REMOTE.get().unwrap()).await {
            Ok(remote) => {
                let mut resp = Vec::from(SOCKS_RESP_PREFIX.as_ref());
                if let SocketAddr::V4(addr) = remote.local_addr().as_ref().unwrap() {
                    resp.extend_from_slice(&addr.ip().octets());
                    resp.extend_from_slice(&addr.port().to_be_bytes());
                    self.inbound.write_all(&resp).await?;
                }
                remote
            }
            Err(e) => {
                let err_type = match e.kind() {
                    ErrorKind::ConnectionRefused => 0x05,
                    ErrorKind::ConnectionAborted => 0x04,
                    _ => 0x03,
                };
                self.inbound
                    .write_all(&[
                        0x05, err_type, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    ])
                    .await?;
                return Err(anyhow!(
                    "error connect to server {}. err: {:?}",
                    REMOTE.get().unwrap(),
                    e
                ));
            }
        };

        remote.set_nodelay(true)?;
        self.outbound = Some(remote);

        let size = self.host.len() as u8;
        self.outbound_mut().write_all(&size.to_be_bytes()).await?;

        let mut header = self.host.clone();
        header.extend_from_slice(&self.port);
        trace!("size: {}, header: {:?}", size, &header);
        self.outbound_mut().write_all(&header).await?;

        debug!(
            "relay {} <-> {}:{} established",
            self.outbound_local_addr(),
            self.normalized_host(),
            self.normalized_port()
        );

        Ok(())
    }

    fn relay(&mut self) -> Relay<TcpStream, TcpStream> {
        let inbound_addr = format!(
            "browser({}:{})",
            self.normalized_host(),
            self.normalized_port()
        );
        let outbound_addr = format!("server({})", self.outbound_local_addr());
        let ri = self.inbound.clone();
        let wi = self.inbound.clone();
        let ro = self.outbound.as_ref().unwrap().clone();
        let wo = self.outbound.as_ref().unwrap().clone();
        Relay::new(
            Channel::new(ri, inbound_addr.clone(), wo, outbound_addr.clone()),
            Channel::new(ro, outbound_addr, wi, inbound_addr),
        )
    }

    fn normalized_host(&self) -> &str {
        std::str::from_utf8(&self.host).unwrap()
    }

    fn normalized_port(&self) -> u16 {
        u16::from_be_bytes(self.port)
    }

    fn outbound_mut(&mut self) -> &mut TcpStream {
        self.outbound.as_mut().unwrap()
    }

    fn outbound_local_addr(&self) -> SocketAddr {
        self.outbound.as_ref().unwrap().local_addr().unwrap()
    }
}

fn validate_version(buf: &[u8]) -> Result<(), anyhow::Error> {
    if buf[0] != 0x05 {
        return Err(anyhow!("unknown version {}", buf[0]));
    }

    Ok(())
}
