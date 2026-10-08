//! TCP for the line-based transports.
//!
//! TeamSpeak sends a reply in more than one segment and waits for the
//! client's ACK of the first before it sends the rest. Linux delays that ACK
//! by about 40 ms, so every query command took about 45 ms. Re-arming
//! `TCP_QUICKACK` after each read that returned data (the kernel clears it
//! by itself) makes the ACK go out at once: a command then takes under 1 ms.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// A TCP connection that acknowledges what it receives at once.
pub(crate) struct QuickAck(TcpStream);

impl QuickAck {
	/// Connect to `addr`, giving up after `timeout`.
	pub(crate) async fn connect(addr: &str, timeout: Duration) -> io::Result<Self> {
		let stream =
			tokio::time::timeout(timeout, TcpStream::connect(addr)).await.map_err(|_| {
				io::Error::new(
					io::ErrorKind::TimedOut,
					format!("no connection to {addr} within {} s", timeout.as_secs()),
				)
			})??;
		stream.set_nodelay(true)?;
		let stream = Self(stream);
		stream.rearm();
		Ok(stream)
	}

	fn rearm(&self) {
		#[cfg(any(target_os = "linux", target_os = "android"))]
		let _ = socket2::SockRef::from(&self.0).set_tcp_quickack(true);
	}
}

impl AsyncRead for QuickAck {
	fn poll_read(
		mut self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &mut ReadBuf<'_>,
	) -> Poll<io::Result<()>> {
		let before = buf.filled().len();
		let poll = Pin::new(&mut self.0).poll_read(cx, buf);
		if matches!(poll, Poll::Ready(Ok(()))) && buf.filled().len() > before {
			self.rearm();
		}
		poll
	}
}

impl AsyncWrite for QuickAck {
	fn poll_write(
		mut self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &[u8],
	) -> Poll<io::Result<usize>> {
		Pin::new(&mut self.0).poll_write(cx, buf)
	}

	fn poll_write_vectored(
		mut self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		bufs: &[io::IoSlice<'_>],
	) -> Poll<io::Result<usize>> {
		Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
	}

	fn is_write_vectored(&self) -> bool {
		self.0.is_write_vectored()
	}

	fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		Pin::new(&mut self.0).poll_flush(cx)
	}

	fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		Pin::new(&mut self.0).poll_shutdown(cx)
	}
}
