use std::env;
use std::future::Future;
use std::net::SocketAddr;

use futures::future::try_join;
use log::{debug, error, info};
use once_cell::sync::Lazy;
use rand::distributions::{Distribution, Uniform};
use rand::prelude::*;
use tokio::io::{copy, AsyncReadExt};
use tokio::net::{lookup_host, TcpListener, TcpStream, ToSocketAddrs};
use tokio::time::delay_for;
use tokio::time::Duration;

use anyhow::{anyhow, Context};
use sagittarius::DEFAULT_SERVER_ADDR;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    sagittarius::init_log();

    let listen_addr = env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_SERVER_ADDR.to_string());

    accept_loop(listen_addr).await
}

async fn accept_loop(addr: impl ToSocketAddrs) -> Result<(), anyhow::Error> {
    let mut listener = TcpListener::bind(addr).await?;
    info!("Listening on: {:?}", listener.local_addr().unwrap());

    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                debug!("accept request from {:?}", addr);

                tokio::spawn(async move {
                    let mut sess = Session {
                        inbound: socket,
                        peer_addr: addr,
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

    target_addr: Option<String>,
    outbound: Option<TcpStream>,
}

static UNIFORM: Lazy<Uniform<u64>> = Lazy::new(|| Uniform::new(100, 5000));

impl Session {
    async fn run(&mut self) -> Result<(), anyhow::Error> {
        self.read_head().await?;
        self.transfer().await?;
        Ok(())
    }

    async fn read_head(&mut self) -> Result<(), anyhow::Error> {
        let size = random_wait_on_fail(self.inbound.read_u8()).await?;

        let mut buf = Vec::new();
        buf.resize(size as usize, 0);
        random_wait_on_fail(self.inbound.read_exact(&mut buf)).await?;
        debug!("size: {}, header: {:?}", size, &buf);

        let host = String::from_utf8(buf)?;
        let port = self.inbound.read_u16().await?;
        self.target_addr = Some(format!("{}:{}", host, port));
        debug!(
            "request to {} from {:?}",
            self.target_addr.as_ref().unwrap(),
            self.peer_addr
        );
        Ok(())
    }

    async fn transfer(&mut self) -> Result<(), anyhow::Error> {
        let addr = self.target_addr().to_owned();
        let f = || format!("address: {}", addr);
        let addrs = lookup_host(self.target_addr()).await.with_context(&f)?;
        let addr = addrs.choose(&mut rand::thread_rng());

        if let Some(addr) = addr {
            self.outbound = Some(TcpStream::connect(addr).await.with_context(&f)?);
            debug!(
                "resolved {} to {:?}",
                self.target_addr(),
                self.outbound.as_ref().unwrap().peer_addr().unwrap()
            );

            let (mut ri, mut wi) = self.inbound.split();
            let (mut ro, mut wo) = self.outbound.as_mut().unwrap().split();

            let i2o = copy(&mut ri, &mut wo);
            let o2i = copy(&mut ro, &mut wi);
            try_join(i2o, o2i).await.with_context(&f)?;
            Ok(())
        } else {
            Err(anyhow!("resolve {} to empty", self.target_addr()))
        }
    }

    fn target_addr(&self) -> &str {
        self.target_addr.as_ref().unwrap()
    }
}

async fn random_wait_on_fail<R, E>(f: impl Future<Output = Result<R, E>>) -> Result<R, E> {
    let r = f.await;

    if let Err(_) = &r {
        let millis = UNIFORM.sample(&mut rand::thread_rng());
        delay_for(Duration::from_millis(millis)).await;
    }
    r
}
