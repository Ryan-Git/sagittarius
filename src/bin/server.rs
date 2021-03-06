use std::env;
use std::future::Future;
use std::net::SocketAddr;

use log::{debug, error, info, trace};
use lru::LruCache;
use once_cell::sync::Lazy;
use rand::distributions::{Distribution, Uniform};
use rand::prelude::*;
use tokio::io::AsyncReadExt;
use tokio::net::{lookup_host, TcpListener, TcpStream, ToSocketAddrs};
use tokio::time::{delay_for, Duration};

use anyhow::anyhow;
use sagittarius::{Channel, Relay, DEFAULT_SERVER_ADDR};
use tokio::net::tcp::{ReadHalf, WriteHalf};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    sagittarius::init_log();

    let listen_addr = env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_SERVER_ADDR.to_string());

    accept_loop(listen_addr).await
}

const TIMEOUT: Duration = Duration::from_secs(15);

async fn accept_loop(addr: impl ToSocketAddrs) -> Result<(), anyhow::Error> {
    let mut listener = TcpListener::bind(addr).await?;
    info!("Listening on: {}", listener.local_addr().unwrap());

    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                socket.set_keepalive(Some(TIMEOUT))?;
                socket.set_nodelay(true)?;

                tokio::spawn(async move {
                    let mut sess = Session {
                        inbound: socket,
                        peer_addr: addr,
                        addr_cache: LruCache::new(128),
                        target_addr: None,
                        outbound: None,
                    };
                    if let Err(e) = sess.run().await {
                        error!("Error: {:?}", e);
                    }
                });
            }
            Err(e) => {
                error!("Accept error: {:?}. Sleeping 0.5s.", e);
                delay_for(Duration::from_millis(500)).await;
            }
        }
    }
}

#[derive(Debug)]
struct Session {
    inbound: TcpStream,
    peer_addr: SocketAddr,
    addr_cache: LruCache<String, SocketAddr>,

    target_addr: Option<String>,
    outbound: Option<TcpStream>,
}

static UNIFORM: Lazy<Uniform<u64>> = Lazy::new(|| Uniform::new(100, 5000));

impl Session {
    async fn run(&mut self) -> Result<(), anyhow::Error> {
        self.read_address().await?;
        self.relay().await
    }

    async fn read_address(&mut self) -> Result<(), anyhow::Error> {
        let size = random_wait_on_fail(self.inbound.read_u8()).await?;

        let mut buf = Vec::new();
        buf.resize(size as usize, 0);
        random_wait_on_fail(self.inbound.read_exact(&mut buf)).await?;
        trace!("size: {}, header: {:?}", size, &buf);

        let host = String::from_utf8(buf)?;
        let port = self.inbound.read_u16().await?;
        self.target_addr = Some(format!("{}:{}", host, port));

        let remote_ip = match self.addr_cache.get(self.target_addr.as_ref().unwrap()) {
            Some(addr) => addr.clone(),
            None => match lookup_host(self.target_addr()).await {
                Ok(addrs) => addrs
                    .filter(|s| s.is_ipv4())
                    .choose(&mut rand::thread_rng())
                    .expect("at least one address"),
                Err(e) => {
                    return Err(anyhow!(
                        "fail to resolve {}. err: {:?}",
                        self.target_addr(),
                        e
                    ));
                }
            },
        };

        trace!(
            "RELAY {} <-> {}({}) establishing",
            self.peer_addr,
            self.target_addr(),
            remote_ip
        );

        self.addr_cache
            .put(self.target_addr.as_ref().unwrap().clone(), remote_ip);

        match TcpStream::connect(remote_ip).await {
            Ok(remote) => {
                remote.set_keepalive(Some(TIMEOUT))?;
                remote.set_nodelay(true)?;
                self.outbound = Some(remote);
            }
            Err(err) => {
                self.addr_cache.pop(self.target_addr.as_ref().unwrap());
                return Err(anyhow!(
                    "failed to connect remote {}({}), {}",
                    self.target_addr(),
                    remote_ip,
                    err
                ));
            }
        };

        debug!(
            "RELAY {} <-> {}({}) established",
            self.peer_addr,
            self.target_addr(),
            remote_ip
        );
        Ok(())
    }

    fn relay(&mut self) -> Relay<ReadHalf<'_>, WriteHalf<'_>> {
        let out_addr = format!(
            "{}({})",
            self.target_addr(),
            self.outbound.as_ref().unwrap().peer_addr().unwrap()
        );

        let (ri, wi) = self.inbound.split();
        let (ro, wo) = self.outbound.as_mut().unwrap().split();
        Relay::new(
            Channel::new(ri, self.peer_addr.to_string(), wo, out_addr.clone()),
            Channel::new(ro, out_addr, wi, self.peer_addr.to_string()),
        )
    }

    fn target_addr(&self) -> &str {
        self.target_addr.as_ref().unwrap()
    }
}

async fn random_wait_on_fail<R, E>(f: impl Future<Output = Result<R, E>>) -> Result<R, E> {
    let r = f.await;

    if let Err(_) = &r {
        let millis = UNIFORM.sample(&mut rand::thread_rng());
        debug!("delay error for {} ms", millis);
        delay_for(Duration::from_millis(millis)).await;
    }
    r
}
