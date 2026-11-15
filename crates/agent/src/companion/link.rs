// SPDX-License-Identifier: GPL-3.0-or-later

//! zbus socket over a byte stream, such as the companion's named pipe.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;
use zbus::AuthMechanism;
use zbus::connection::socket::{BoxedSplit, ReadHalf, WriteHalf};

#[derive(Debug)]
struct Read<R> {
    inner: R,
    /// Fired once the stream ends.
    closed: Option<oneshot::Sender<()>>,
}

#[derive(Debug)]
struct Write<W>(W);

/// The zbus halves of `stream`. `closed` fires when the other end goes
/// away.
pub fn split<S>(stream: S, closed: oneshot::Sender<()>) -> BoxedSplit
where
    S: AsyncRead + AsyncWrite + Send + Sync + Unpin + std::fmt::Debug + 'static,
{
    let (r, w) = tokio::io::split(stream);
    let read = Read {
        inner: r,
        closed: Some(closed),
    };
    BoxedSplit::new(Box::new(read), Box::new(Write(w)))
}

#[async_trait::async_trait]
impl<R> ReadHalf for Read<R>
where
    R: AsyncRead + Send + Sync + Unpin + std::fmt::Debug + 'static,
{
    async fn recvmsg(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let res = self.inner.read(buf).await;
        if matches!(res, Ok(0) | Err(_))
            && let Some(closed) = self.closed.take()
        {
            let _ = closed.send(());
        }
        res
    }

    fn auth_mechanism(&self) -> AuthMechanism {
        AuthMechanism::Anonymous
    }
}

#[async_trait::async_trait]
impl<W> WriteHalf for Write<W>
where
    W: AsyncWrite + Send + Sync + Unpin + std::fmt::Debug + 'static,
{
    async fn sendmsg(
        &mut self,
        buf: &[u8],
        #[cfg(unix)] _fds: &[std::os::fd::BorrowedFd<'_>],
    ) -> io::Result<usize> {
        self.0.write_all(buf).await?;
        Ok(buf.len())
    }

    async fn close(&mut self) -> io::Result<()> {
        self.0.shutdown().await
    }
}
