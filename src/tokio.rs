// From https://github.com/little-dude/netlink/tree/master/netlink-sys/

use std::{
    io,
    task::{Context, Poll},
    os::unix::io::{FromRawFd, RawFd}
};

use futures::{future::poll_fn, ready};
// use log::trace;
use mio;
use tokio::io::PollEvented;

use crate::socket::{SocketAddr, Socket as InnerSocket};

/// An I/O object representing a Netlink socket.
/// This custom implementation only borrows the sockets, it does not close() on drop
pub struct AsyncSocket(PollEvented<InnerSocket>);

impl AsyncSocket {
    pub fn new(fd: RawFd) -> io::Result<Self> {
        let socket = unsafe { InnerSocket::from_raw_fd(fd) };
        socket.set_non_blocking(true)?;
        Ok(AsyncSocket(PollEvented::new(socket)?))
    }

    pub async fn send(&mut self, buf: &[u8]) -> io::Result<usize> {
        poll_fn(|cx| {
            // Check if the socket it writable. If
            // PollEvented::poll_write_ready returns NotReady, it will
            // already have arranged for the current task to be
            // notified when the socket becomes writable, so we can
            // just return Pending
            ready!(self.0.poll_write_ready(cx))?;

            match self.0.get_ref().send(buf, 0) {
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.0.clear_write_ready(cx)?;
                    Poll::Pending
                }
                x => Poll::Ready(x),
            }
        })
        .await
    }

    pub async fn send_to(&mut self, buf: &[u8], addr: &SocketAddr) -> io::Result<usize> {
        poll_fn(|cx| self.poll_send_to(cx, buf, addr)).await
    }

    pub fn poll_send_to(
        &mut self,
        cx: &mut Context,
        buf: &[u8],
        addr: &SocketAddr,
    ) -> Poll<io::Result<usize>> {
        ready!(self.0.poll_write_ready(cx))?;
        match self.0.get_ref().send_to(buf, addr, 0) {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.0.clear_write_ready(cx)?;
                Poll::Pending
            }
            x => Poll::Ready(x),
        }
    }

    pub async fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| {
            // Check if the socket is readable. If not,
            // PollEvented::poll_read_ready would have arranged for the
            // current task to be polled again when the socket becomes
            // readable, so we can just return Pending
            ready!(self.0.poll_read_ready(cx, mio::Ready::readable()))?;

            match self.0.get_ref().recv(buf, 0) {
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // If the socket is not readable, make sure the
                    // current task get notified when the socket becomes
                    // readable again.
                    self.0.clear_read_ready(cx, mio::Ready::readable())?;
                    Poll::Pending
                }
                x => Poll::Ready(x),
            }
        })
        .await
    }

    pub async fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        poll_fn(|cx| self.poll_recv_from(cx, buf)).await
    }

    pub fn poll_recv_from(
        &mut self,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        // trace!("poll_recv_from called");
        ready!(self.0.poll_read_ready(cx, mio::Ready::readable()))?;

        // trace!("poll_recv_from socket is ready for reading");
        match self.0.get_ref().recv_from(buf, 0) {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // trace!("poll_recv_from socket would block");
                self.0.clear_read_ready(cx, mio::Ready::readable())?;
                Poll::Pending
            }
            x => {
                // trace!("poll_recv_from {:?} bytes read", x);
                Poll::Ready(x)
            }
        }
    }
}
