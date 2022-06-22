// From https://github.com/little-dude/netlink/tree/master/netlink-sys/
//! Netlink socket related functions
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::{self, Error, Result};
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::task::{Context, Poll};
use netlink_sys::TokioSocket;

#[derive(Clone, Debug)]
pub struct Socket(RawFd);

impl AsRawFd for Socket {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl FromRawFd for Socket {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Socket(fd)
    }
}

// impl Drop for Socket {
//     fn drop(&mut self) {
//         unsafe { libc::close(self.0) };
//     }
// }

#[derive(Copy, Clone)]
pub struct SocketAddr(libc::sockaddr_nl);

impl Hash for SocketAddr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.nl_family.hash(state);
        self.0.nl_pid.hash(state);
        self.0.nl_groups.hash(state);
    }
}

impl PartialEq for SocketAddr {
    fn eq(&self, other: &SocketAddr) -> bool {
        self.0.nl_family == other.0.nl_family
            && self.0.nl_pid == other.0.nl_pid
            && self.0.nl_groups == other.0.nl_groups
    }
}

impl fmt::Debug for SocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SocketAddr(nl_family={}, nl_pid={}, nl_groups={})",
            self.0.nl_family, self.0.nl_pid, self.0.nl_groups
        )
    }
}

impl Eq for SocketAddr {}

impl fmt::Display for SocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "address family: {}, pid: {}, multicast groups: {})",
            self.0.nl_family, self.0.nl_pid, self.0.nl_groups
        )
    }
}

impl SocketAddr {
    pub fn new() -> Self {
        let mut addr: libc::sockaddr_nl = unsafe { mem::zeroed() };
        addr.nl_family = libc::PF_NETLINK as libc::sa_family_t;
        SocketAddr(addr)
    }

    fn as_raw(&self) -> (*const libc::sockaddr, libc::socklen_t) {
        let addr_ptr = &self.0 as *const libc::sockaddr_nl as *const libc::sockaddr;
        let addr_len = mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        (addr_ptr, addr_len)
    }
}

impl Socket {
    pub fn set_non_blocking(&self, non_blocking: bool) -> Result<()> {
        let mut non_blocking = non_blocking as libc::c_int;
        let res = unsafe { libc::ioctl(self.0, libc::FIONBIO, &mut non_blocking) };
        if res < 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }
}


/// An I/O object representing a Netlink socket.
/// This custom implementation only borrows the sockets, it does not close() on drop
pub struct AsyncSocket(TokioSocket);

impl AsyncSocket {
    pub fn new(fd: RawFd) -> Self {
        let socket = unsafe { TokioSocket::from_raw_fd(fd) };
        Ok(AsyncSocket(socket))
    }

    pub async fn send_to(&mut self, buf: &[u8], addr: &SocketAddr) -> io::Result<usize> {
        poll_fn(|cx| self.0.poll_send_to(cx, buf, addr)).await
    }

    pub async fn recv(&self, buf: &mut [u8], flags: libc::c_int) -> io::Result<usize> {
        poll_fn(|cx| {
            // Check if the socket is readable. If not,
            // PollEvented::poll_read_ready would have arranged for the
            // current task to be polled again when the socket becomes
            // readable, so we can just return Pending
            ready!(self.0.poll_read_ready(cx, mio::Ready::readable()))?;

            // TODO: Is it save to pass flags in non-blocking mode?
            match self.0.get_ref().recv(buf, flags) {
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
}
