// From https://github.com/little-dude/netlink/tree/master/netlink-sys/
//! Netlink socket related functions
use libc;
use mio::event::Evented;
use mio::unix::EventedFd;
use futures::{future::poll_fn, ready};
use tokio::io::PollEvented;

use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::{self, Error, Result};
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::task::{Context, Poll};

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

    fn as_raw_mut(&mut self) -> (*mut libc::sockaddr, libc::socklen_t) {
        let addr_ptr = &mut self.0 as *mut libc::sockaddr_nl as *mut libc::sockaddr;
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

    pub fn connect(&self, remote_addr: &SocketAddr) -> Result<()> {
        // Event though for SOCK_DGRAM sockets there's no IO, since our socket is non-blocking,
        // connect() might return EINPROGRESS. In theory, the right way to treat EINPROGRESS would
        // be to ignore the error, and let the user poll the socket to check when it becomes
        // writable, indicating that the connection succeeded. The code already exists in mio for
        // TcpStream:
        //
        // > pub fn connect(stream: net::TcpStream, addr: &SocketAddr) -> io::Result<TcpStream> {
        // >     set_non_block(stream.as_raw_fd())?;
        // >     match stream.connect(addr) {
        // >         Ok(..) => {}
        // >         Err(ref e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {}
        // >         Err(e) => return Err(e),
        // >     }
        // >     Ok(TcpStream {  inner: stream })
        // > }
        //
        // The polling to wait for the connection is available in the tokio-tcp crate. See:
        // https://github.com/tokio-rs/tokio/blob/363b207f2b6c25857c70d76b303356db87212f59/tokio-tcp/src/stream.rs#L706
        //
        // In practice, since the connection does not require any IO for SOCK_DGRAM sockets, it
        // almost never returns EINPROGRESS and so for now, we just return whatever libc::connect
        // returns. If it returns EINPROGRESS, the caller will have to handle the error themself
        //
        // Refs:
        //
        // - https://stackoverflow.com/a/14046386/1836144
        // - https://lists.isc.org/pipermail/bind-users/2009-August/077527.html
        let (addr, addr_len) = remote_addr.as_raw();
        let res = unsafe { libc::connect(self.0, addr, addr_len) };
        if res < 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }

    // Most of the comments in this method come from a discussion on rust users forum.
    // [thread]: https://users.rust-lang.org/t/help-understanding-libc-call/17308/9
    //
    // WARNING: with datagram oriented protocols, `recv` and
    // `recvfrom` receive normally only ONE datagram, but it seems not
    // to be verified for Netlink sockets: multiple message can be
    // received in a single call.
    pub fn recv_from(&self, buf: &mut [u8], flags: libc::c_int) -> Result<(usize, SocketAddr)> {
        // Create an empty storage for the address. Note that Rust standard library create a
        // sockaddr_storage so that it works for any address family, but here, we already know that
        // we'll have a Netlink address, so we can create the appropriate storage.
        let mut addr = unsafe { mem::zeroed::<libc::sockaddr_nl>() };

        // recvfrom takes a *sockaddr as parameter so that it can accept any kind of address
        // storage, so we need to create such a pointer for the sockaddr_nl we just initialized.
        //
        //                     Create a raw pointer to        Cast our raw pointer to a
        //                     our storage. We cannot         generic pointer to *sockaddr
        //                     pass it to recvfrom yet.       that recvfrom can use
        //                                 ^                              ^
        //                                 |                              |
        //                  +--------------+---------------+    +---------+--------+
        //                 /                                \  /                    \
        let addr_ptr = &mut addr as *mut libc::sockaddr_nl as *mut libc::sockaddr;

        // Why do we need to pass the address length? We're passing a generic *sockaddr to
        // recvfrom. Somehow recvfrom needs to make sure that the address of the received packet
        // would fit into the actual type that is behind *sockaddr: it could be a sockaddr_nl but
        // also a sockaddr_in, a sockaddr_in6, or even the generic sockaddr_storage that can store
        // any address.
        let mut addrlen = mem::size_of_val(&addr);
        // recvfrom does not take the address length by value (see [thread]), so we need to create
        // a pointer to it.
        let addrlen_ptr = &mut addrlen as *mut usize as *mut libc::socklen_t;

        //                      Cast the *mut u8 into *mut void.
        //               This is equivalent to casting a *char into *void
        //                                 See [thread]
        //                                       ^
        //           Create a *mut u8            |
        //                   ^                   |
        //                   |                   |
        //             +-----+-----+    +--------+-------+
        //            /             \  /                  \
        let buf_ptr = buf.as_mut_ptr() as *mut libc::c_void;
        let buf_len = buf.len() as libc::size_t;

        let res = unsafe { libc::recvfrom(self.0, buf_ptr, buf_len, flags, addr_ptr, addrlen_ptr) };
        if res < 0 {
            return Err(Error::last_os_error());
        }
        Ok((res as usize, SocketAddr(addr)))
    }

    pub fn recv(&self, buf: &mut [u8], flags: libc::c_int) -> Result<usize> {
        let buf_ptr = buf.as_mut_ptr() as *mut libc::c_void;
        let buf_len = buf.len() as libc::size_t;

        let res = unsafe { libc::recv(self.0, buf_ptr, buf_len, flags) };
        if res < 0 {
            return Err(Error::last_os_error());
        }
        Ok(res as usize)
    }

    pub fn send_to(&self, buf: &[u8], addr: &SocketAddr, flags: libc::c_int) -> Result<usize> {
        let (addr_ptr, addr_len) = addr.as_raw();
        let buf_ptr = buf.as_ptr() as *const libc::c_void;
        let buf_len = buf.len() as libc::size_t;

        let res = unsafe { libc::sendto(self.0, buf_ptr, buf_len, flags, addr_ptr, addr_len) };
        if res < 0 {
            return Err(Error::last_os_error());
        }
        Ok(res as usize)
    }

    pub fn send(&self, buf: &[u8], flags: libc::c_int) -> Result<usize> {
        let buf_ptr = buf.as_ptr() as *const libc::c_void;
        let buf_len = buf.len() as libc::size_t;

        let res = unsafe { libc::send(self.0, buf_ptr, buf_len, flags) };
        if res < 0 {
            return Err(Error::last_os_error());
        }
        Ok(res as usize)
    }
}

/// Implement mio
impl Evented for Socket {
    fn register(
        &self,
        poll: &mio::Poll,
        token: mio::Token,
        interest: mio::Ready,
        opts: mio::PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.as_raw_fd()).register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &mio::Poll,
        token: mio::Token,
        interest: mio::Ready,
        opts: mio::PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.as_raw_fd()).reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        EventedFd(&self.as_raw_fd()).deregister(poll)
    }
}

/// An I/O object representing a Netlink socket.
/// This custom implementation only borrows the sockets, it does not close() on drop
pub struct AsyncSocket(PollEvented<Socket>);

impl AsyncSocket {
    pub fn new(fd: RawFd) -> io::Result<Self> {
        let socket = unsafe { Socket::from_raw_fd(fd) };
        socket.set_non_blocking(true)?;
        Ok(AsyncSocket(PollEvented::new(socket)?))
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

    pub async fn recv(&mut self, buf: &mut [u8], flags: libc::c_int) -> io::Result<usize> {
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
