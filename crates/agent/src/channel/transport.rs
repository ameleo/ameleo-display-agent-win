// SPDX-License-Identifier: GPL-3.0-only

//! zbus socket over the virtio-serial port, one instance per D-Bus session.

use std::io;

use tokio::sync::mpsc;
use zbus::AuthMechanism;
use zbus::connection::socket::{BoxedSplit, ReadHalf, WriteHalf};

pub type Split = BoxedSplit;

use crate::channel::port::Outgoing;

#[derive(Debug)]
pub struct SessionRead {
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

#[derive(Debug)]
pub struct SessionWrite {
    tx: mpsc::Sender<Outgoing>,
    generation: u64,
}

/// Builds the zbus halves for session `generation`. Bytes for the session
/// are pushed into the returned sender, closing it ends the session.
pub fn session(
    generation: u64,
    out: mpsc::Sender<Outgoing>,
) -> (Split, mpsc::UnboundedSender<Vec<u8>>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let read = SessionRead {
        rx,
        buf: Vec::new(),
        pos: 0,
    };
    let write = SessionWrite {
        tx: out,
        generation,
    };
    (BoxedSplit::new(Box::new(read), Box::new(write)), tx)
}

#[async_trait::async_trait]
impl ReadHalf for SessionRead {
    async fn recvmsg(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.buf.len() {
            match self.rx.recv().await {
                Some(data) => {
                    self.buf = data;
                    self.pos = 0;
                }
                None => return Ok(0), // session closed: EOF
            }
        }
        let n = buf.len().min(self.buf.len() - self.pos);
        buf[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }

    fn auth_mechanism(&self) -> AuthMechanism {
        AuthMechanism::Anonymous
    }
}

#[async_trait::async_trait]
impl WriteHalf for SessionWrite {
    async fn sendmsg(
        &mut self,
        buf: &[u8],
        #[cfg(unix)] _fds: &[std::os::fd::BorrowedFd<'_>],
    ) -> io::Result<usize> {
        self.tx
            .send((self.generation, buf.to_vec()))
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        Ok(buf.len())
    }

    async fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}
