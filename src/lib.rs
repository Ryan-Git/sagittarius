use std::future::Future;
use std::io;
use std::io::Write;
use std::pin::Pin;
use std::task::{Context, Poll};

use chrono::prelude::*;
use env_logger::Env;
use log::{debug, error, trace};

pub const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:18888";

pub fn init_log() {
    const DEFAULT_LOG_LEVEL: &str = "debug";

    env_logger::from_env(Env::default().default_filter_or(DEFAULT_LOG_LEVEL))
        .format(|buf, record| {
            writeln!(
                buf,
                "[{} {:<5} {}:{}] {}",
                Local::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                record.level(),
                record.file().unwrap(),
                record.line().unwrap(),
                record.args()
            )
        })
        .init();
}

pub trait AsyncRead {
    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>>;
}

impl AsyncRead for tokio::net::tcp::ReadHalf<'_> {
    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let me = Pin::new(self);
        return tokio::io::AsyncRead::poll_read(me, cx, buf);
    }
}

impl AsyncRead for async_std::net::TcpStream {
    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let me = Pin::new(self);
        return async_std::io::Read::poll_read(me, cx, buf);
    }
}

pub trait AsyncWrite {
    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>>;

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
}

impl AsyncWrite for tokio::net::tcp::WriteHalf<'_> {
    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let me = Pin::new(self);
        return tokio::io::AsyncWrite::poll_write(me, cx, buf);
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = Pin::new(self);
        return tokio::io::AsyncWrite::poll_flush(me, cx);
    }
}

impl AsyncWrite for async_std::net::TcpStream {
    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let me = Pin::new(self);
        return async_std::io::Write::poll_write(me, cx, buf);
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = Pin::new(self);
        return async_std::io::Write::poll_flush(me, cx);
    }
}

const BUF_SIZE: usize = 8192;

pub struct Channel<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    buf: [u8; BUF_SIZE],
    pos: usize,
    cap: usize,
    amt: u64,
    read_done: bool,
    read: R,
    read_addr: String,
    write: W,
    write_addr: String,
}

impl<R, W> Channel<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn new(read: R, read_addr: String, write: W, write_addr: String) -> Channel<R, W> {
        Channel {
            buf: [0; BUF_SIZE],
            pos: 0,
            cap: 0,
            amt: 0,
            read_done: false,
            read,
            read_addr,
            write,
            write_addr,
        }
    }
}

impl<R, W> Future for Channel<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    type Output = Result<u64, std::io::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            if self.pos == self.cap && !self.read_done {
                let me = &mut *self;

                match me.read.poll_read(cx, &mut me.buf) {
                    Poll::Ready(Ok(n)) if n == 0 => {
                        self.read_done = true;
                    }
                    Poll::Ready(Ok(n)) => {
                        self.pos = 0;
                        self.cap = n;
                        debug!("{} -> {} read {} bytes", self.read_addr, self.write_addr, n);
                    }
                    Poll::Ready(Err(e)) => {
                        error!(
                            "{} -> {} read error: {}",
                            self.read_addr, self.write_addr, &e
                        );
                        return Poll::Ready(Err(e));
                    }
                    Poll::Pending => {
                        trace!("{} -> {} read pending", self.read_addr, self.write_addr);
                        return Poll::Pending;
                    }
                }
            }

            while self.pos < self.cap {
                let me = &mut *self;

                match me.write.poll_write(cx, &me.buf[me.pos..me.cap]) {
                    Poll::Ready(Ok(n)) if n == 0 => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "write zero byte into writer",
                        )));
                    }
                    Poll::Ready(Ok(n)) => {
                        debug!(
                            "{} -> {} write {} bytes",
                            self.read_addr, self.write_addr, n
                        );
                        self.pos += n;
                        self.amt += n as u64;
                    }
                    Poll::Ready(Err(e)) => {
                        error!(
                            "{} -> {} write error: {}",
                            self.read_addr, self.write_addr, &e
                        );
                        return Poll::Ready(Err(e));
                    }
                    Poll::Pending => {
                        trace!("{} -> {} write pending", self.read_addr, self.write_addr);
                        return Poll::Pending;
                    }
                }
            }

            if self.pos == self.cap && self.read_done {
                let me = &mut *self;

                return match me.write.poll_flush(cx) {
                    Poll::Ready(Ok(())) => {
                        debug!(
                            "{} -> {} written {} bytes closed",
                            self.read_addr, self.write_addr, self.amt
                        );
                        Poll::Ready(Ok(self.amt))
                    }
                    Poll::Ready(Err(e)) => {
                        error!(
                            "{} -> {} flush error: {}",
                            self.read_addr, self.write_addr, &e
                        );
                        Poll::Ready(Err(e))
                    }
                    Poll::Pending => {
                        trace!("{} -> {} flush pending", self.read_addr, self.write_addr);
                        Poll::Pending
                    }
                };
            }
        }
    }
}

pub struct Relay<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    i2o: Channel<R, W>,
    i2o_close: bool,
    o2i: Channel<R, W>,
    o2i_close: bool,
}

impl<'a, R, W> Relay<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn new(i2o: Channel<R, W>, o2i: Channel<R, W>) -> Relay<R, W> {
        Relay {
            i2o,
            i2o_close: false,
            o2i,
            o2i_close: false,
        }
    }
}

impl<R, W> Future for Relay<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    type Output = Result<(), anyhow::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.i2o_close {
            let i2o = Pin::new(&mut self.i2o);
            match i2o.poll(cx) {
                Poll::Ready(_) => {
                    self.i2o_close = true;
                }
                Poll::Pending => {}
            }
        }

        if !self.o2i_close {
            let o2i = Pin::new(&mut self.o2i);
            match o2i.poll(cx) {
                Poll::Ready(_) => {
                    self.i2o_close = true;
                }
                Poll::Pending => {}
            }
        }

        if self.i2o_close && self.o2i_close {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}
