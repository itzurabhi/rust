use crate::cmp;
use crate::ffi::CStr;
use crate::io::{self, IoSlice, IoSliceMut};
use crate::mem;
use crate::net::{Shutdown, SocketAddr};
use crate::os::unix::io::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, RawFd};
use crate::str;
use crate::sys::fd::FileDesc;
use crate::sys_common::net::{getsockopt, setsockopt, sockaddr_to_addr};
use crate::sys_common::{AsInner, FromInner, IntoInner};
use crate::time::{Duration, Instant};

use libc::{c_int, c_void, size_t, sockaddr, socklen_t, MSG_PEEK};

cfg_if::cfg_if! {
    if #[cfg(target_vendor = "apple")] {
        use libc::SO_LINGER_SEC as SO_LINGER;
    } else {
        use libc::SO_LINGER;
    }
}

pub use crate::sys::{cvt, cvt_r};

#[allow(unused_extern_crates)]
pub extern crate libc as netc;

pub type wrlen_t = size_t;

pub struct Socket(FileDesc);

pub fn init() {}

pub fn cvt_gai(err: c_int) -> io::Result<()> {
    if err == 0 {
        return Ok(());
    }

    // We may need to trigger a glibc workaround. See on_resolver_failure() for details.
    on_resolver_failure();

    #[cfg(not(target_os = "espidf"))]
    if err == libc::EAI_SYSTEM {
        return Err(io::Error::last_os_error());
    }

    #[cfg(not(target_os = "espidf"))]
    let detail = unsafe {
        str::from_utf8(CStr::from_ptr(libc::gai_strerror(err)).to_bytes()).unwrap().to_owned()
    };

    #[cfg(target_os = "espidf")]
    let detail = "";

    Err(io::Error::new(
        io::ErrorKind::Uncategorized,
        &format!("failed to lookup address information: {}", detail)[..],
    ))
}

impl Socket {
    pub fn new(addr: &SocketAddr, ty: c_int) -> io::Result<Socket> {
        let fam = match *addr {
            SocketAddr::V4(..) => libc::AF_INET,
            SocketAddr::V6(..) => libc::AF_INET6,
        };
        Socket::new_raw(fam, ty)
    }

    pub fn new_raw(fam: c_int, ty: c_int) -> io::Result<Socket> {
        unsafe {
            cfg_if::cfg_if! {
                if #[cfg(any(
                    target_os = "android",
                    target_os = "dragonfly",
                    target_os = "freebsd",
                    target_os = "illumos",
                    target_os = "linux",
                    target_os = "netbsd",
                    target_os = "openbsd",
                ))] {
                    // On platforms that support it we pass the SOCK_CLOEXEC
                    // flag to atomically create the socket and set it as
                    // CLOEXEC. On Linux this was added in 2.6.27.
                    let fd = cvt(libc::socket(fam, ty | libc::SOCK_CLOEXEC, 0))?;
                    Ok(Socket(FileDesc::from_raw_fd(fd)))
                } else {
                    let fd = cvt(libc::socket(fam, ty, 0))?;
                    let fd = FileDesc::from_raw_fd(fd);
                    fd.set_cloexec()?;
                    let socket = Socket(fd);

                    // macOS and iOS use `SO_NOSIGPIPE` as a `setsockopt`
                    // flag to disable `SIGPIPE` emission on socket.
                    #[cfg(target_vendor = "apple")]
                    setsockopt(&socket, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 1)?;

                    Ok(socket)
                }
            }
        }
    }

    #[cfg(not(target_os = "vxworks"))]
    pub fn new_pair(fam: c_int, ty: c_int) -> io::Result<(Socket, Socket)> {
        unsafe {
            let mut fds = [0, 0];

            cfg_if::cfg_if! {
                if #[cfg(any(
                    target_os = "android",
                    target_os = "dragonfly",
                    target_os = "freebsd",
                    target_os = "illumos",
                    target_os = "linux",
                    target_os = "netbsd",
                    target_os = "openbsd",
                ))] {
                    // Like above, set cloexec atomically
                    cvt(libc::socketpair(fam, ty | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()))?;
                    Ok((Socket(FileDesc::from_raw_fd(fds[0])), Socket(FileDesc::from_raw_fd(fds[1]))))
                } else {
                    cvt(libc::socketpair(fam, ty, 0, fds.as_mut_ptr()))?;
                    let a = FileDesc::from_raw_fd(fds[0]);
                    let b = FileDesc::from_raw_fd(fds[1]);
                    a.set_cloexec()?;
                    b.set_cloexec()?;
                    Ok((Socket(a), Socket(b)))
                }
            }
        }
    }

    #[cfg(target_os = "vxworks")]
    pub fn new_pair(_fam: c_int, _ty: c_int) -> io::Result<(Socket, Socket)> {
        unimplemented!()
    }

    pub fn connect_timeout(&self, addr: &SocketAddr, timeout: Duration) -> io::Result<()> {
        self.set_nonblocking(true)?;
        let r = unsafe {
            let (addrp, len) = addr.into_inner();
            cvt(libc::connect(self.as_raw_fd(), addrp, len))
        };
        self.set_nonblocking(false)?;

        match r {
            Ok(_) => return Ok(()),
            // there's no ErrorKind for EINPROGRESS :(
            Err(ref e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {}
            Err(e) => return Err(e),
        }

        let mut pollfd = libc::pollfd { fd: self.as_raw_fd(), events: libc::POLLOUT, revents: 0 };

        if timeout.as_secs() == 0 && timeout.subsec_nanos() == 0 {
            return Err(io::Error::new_const(
                io::ErrorKind::InvalidInput,
                &"cannot set a 0 duration timeout",
            ));
        }

        let start = Instant::now();

        loop {
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return Err(io::Error::new_const(io::ErrorKind::TimedOut, &"connection timed out"));
            }

            let timeout = timeout - elapsed;
            let mut timeout = timeout
                .as_secs()
                .saturating_mul(1_000)
                .saturating_add(timeout.subsec_nanos() as u64 / 1_000_000);
            if timeout == 0 {
                timeout = 1;
            }

            let timeout = cmp::min(timeout, c_int::MAX as u64) as c_int;

            match unsafe { libc::poll(&mut pollfd, 1, timeout) } {
                -1 => {
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::Interrupted {
                        return Err(err);
                    }
                }
                0 => {}
                _ => {
                    // linux returns POLLOUT|POLLERR|POLLHUP for refused connections (!), so look
                    // for POLLHUP rather than read readiness
                    if pollfd.revents & libc::POLLHUP != 0 {
                        let e = self.take_error()?.unwrap_or_else(|| {
                            io::Error::new_const(
                                io::ErrorKind::Uncategorized,
                                &"no error set after POLLHUP",
                            )
                        });
                        return Err(e);
                    }

                    return Ok(());
                }
            }
        }
    }

    pub fn accept(&self, storage: *mut sockaddr, len: *mut socklen_t) -> io::Result<Socket> {
        // Unfortunately the only known way right now to accept a socket and
        // atomically set the CLOEXEC flag is to use the `accept4` syscall on
        // platforms that support it. On Linux, this was added in 2.6.28,
        // glibc 2.10 and musl 0.9.5.
        cfg_if::cfg_if! {
            if #[cfg(any(
                target_os = "android",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "illumos",
                target_os = "linux",
                target_os = "netbsd",
                target_os = "openbsd",
            ))] {
                unsafe {
                    let fd = cvt_r(|| libc::accept4(self.as_raw_fd(), storage, len, libc::SOCK_CLOEXEC))?;
                    Ok(Socket(FileDesc::from_raw_fd(fd)))
                }
            } else {
                unsafe {
                    let fd = cvt_r(|| libc::accept(self.as_raw_fd(), storage, len))?;
                    let fd = FileDesc::from_raw_fd(fd);
                    fd.set_cloexec()?;
                    Ok(Socket(fd))
                }
            }
        }
    }

    pub fn duplicate(&self) -> io::Result<Socket> {
        self.0.duplicate().map(Socket)
    }

    fn recv_with_flags(&self, buf: &mut [u8], flags: c_int) -> io::Result<usize> {
        let ret = cvt(unsafe {
            libc::recv(self.as_raw_fd(), buf.as_mut_ptr() as *mut c_void, buf.len(), flags)
        })?;
        Ok(ret as usize)
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.recv_with_flags(buf, 0)
    }

    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.recv_with_flags(buf, MSG_PEEK)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        self.0.read_vectored(bufs)
    }

    #[inline]
    pub fn is_read_vectored(&self) -> bool {
        self.0.is_read_vectored()
    }

    fn recv_from_with_flags(
        &self,
        buf: &mut [u8],
        flags: c_int,
    ) -> io::Result<(usize, SocketAddr)> {
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut addrlen = mem::size_of_val(&storage) as libc::socklen_t;

        let n = cvt(unsafe {
            libc::recvfrom(
                self.as_raw_fd(),
                buf.as_mut_ptr() as *mut c_void,
                buf.len(),
                flags,
                &mut storage as *mut _ as *mut _,
                &mut addrlen,
            )
        })?;
        Ok((n as usize, sockaddr_to_addr(&storage, addrlen as usize)?))
    }

    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.recv_from_with_flags(buf, 0)
    }

    #[cfg(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "emscripten",
        target_os = "freebsd",
        target_os = "linux",
        target_os = "netbsd",
        target_os = "openbsd",
    ))]
    pub fn recv_msg(&self, msg: &mut libc::msghdr) -> io::Result<usize> {
        let n = cvt(unsafe { libc::recvmsg(self.as_raw_fd(), msg, libc::MSG_CMSG_CLOEXEC) })?;
        Ok(n as usize)
    }

    pub fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.recv_from_with_flags(buf, MSG_PEEK)
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        self.0.write_vectored(bufs)
    }

    #[inline]
    pub fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }

    #[cfg(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "emscripten",
        target_os = "freebsd",
        target_os = "linux",
        target_os = "netbsd",
        target_os = "openbsd",
    ))]
    pub fn send_msg(&self, msg: &mut libc::msghdr) -> io::Result<usize> {
        let n = cvt(unsafe { libc::sendmsg(self.as_raw_fd(), msg, 0) })?;
        Ok(n as usize)
    }

    pub fn set_timeout(&self, dur: Option<Duration>, kind: libc::c_int) -> io::Result<()> {
        let timeout = match dur {
            Some(dur) => {
                if dur.as_secs() == 0 && dur.subsec_nanos() == 0 {
                    return Err(io::Error::new_const(
                        io::ErrorKind::InvalidInput,
                        &"cannot set a 0 duration timeout",
                    ));
                }

                let secs = if dur.as_secs() > libc::time_t::MAX as u64 {
                    libc::time_t::MAX
                } else {
                    dur.as_secs() as libc::time_t
                };
                let mut timeout = libc::timeval {
                    tv_sec: secs,
                    tv_usec: dur.subsec_micros() as libc::suseconds_t,
                };
                if timeout.tv_sec == 0 && timeout.tv_usec == 0 {
                    timeout.tv_usec = 1;
                }
                timeout
            }
            None => libc::timeval { tv_sec: 0, tv_usec: 0 },
        };
        setsockopt(self, libc::SOL_SOCKET, kind, timeout)
    }

    pub fn timeout(&self, kind: libc::c_int) -> io::Result<Option<Duration>> {
        let raw: libc::timeval = getsockopt(self, libc::SOL_SOCKET, kind)?;
        if raw.tv_sec == 0 && raw.tv_usec == 0 {
            Ok(None)
        } else {
            let sec = raw.tv_sec as u64;
            let nsec = (raw.tv_usec as u32) * 1000;
            Ok(Some(Duration::new(sec, nsec)))
        }
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let how = match how {
            Shutdown::Write => libc::SHUT_WR,
            Shutdown::Read => libc::SHUT_RD,
            Shutdown::Both => libc::SHUT_RDWR,
        };
        cvt(unsafe { libc::shutdown(self.as_raw_fd(), how) })?;
        Ok(())
    }

    pub fn set_linger(&self, linger: Option<Duration>) -> io::Result<()> {
        let linger = libc::linger {
            l_onoff: linger.is_some() as libc::c_int,
            l_linger: linger.unwrap_or_default().as_secs() as libc::c_int,
        };

        setsockopt(self, libc::SOL_SOCKET, SO_LINGER, linger)
    }

    pub fn linger(&self) -> io::Result<Option<Duration>> {
        let val: libc::linger = getsockopt(self, libc::SOL_SOCKET, SO_LINGER)?;

        Ok((val.l_onoff != 0).then(|| Duration::from_secs(val.l_linger as u64)))
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        setsockopt(self, libc::IPPROTO_TCP, libc::TCP_NODELAY, nodelay as c_int)
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        let raw: c_int = getsockopt(self, libc::IPPROTO_TCP, libc::TCP_NODELAY)?;
        Ok(raw != 0)
    }

    #[cfg(any(target_os = "android", target_os = "linux", target_os = "dragonfly",))]
    pub fn set_passcred(&self, passcred: bool) -> io::Result<()> {
        setsockopt(self, libc::SOL_SOCKET, libc::SO_PASSCRED, passcred as libc::c_int)
    }

    #[cfg(any(target_os = "android", target_os = "linux", target_os = "dragonfly",))]
    pub fn passcred(&self) -> io::Result<bool> {
        let passcred: libc::c_int = getsockopt(self, libc::SOL_SOCKET, libc::SO_PASSCRED)?;
        Ok(passcred != 0)
    }

    #[cfg(not(any(target_os = "solaris", target_os = "illumos")))]
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let mut nonblocking = nonblocking as libc::c_int;
        cvt(unsafe { libc::ioctl(self.as_raw_fd(), libc::FIONBIO, &mut nonblocking) }).map(drop)
    }

    #[cfg(any(target_os = "solaris", target_os = "illumos"))]
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        // FIONBIO is inadequate for sockets on illumos/Solaris, so use the
        // fcntl(F_[GS]ETFL)-based method provided by FileDesc instead.
        self.0.set_nonblocking(nonblocking)
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        let raw: c_int = getsockopt(self, libc::SOL_SOCKET, libc::SO_ERROR)?;
        if raw == 0 { Ok(None) } else { Ok(Some(io::Error::from_raw_os_error(raw as i32))) }
    }

    // This is used by sys_common code to abstract over Windows and Unix.
    pub fn as_raw(&self) -> RawFd {
        self.as_raw_fd()
    }
}

impl AsInner<FileDesc> for Socket {
    fn as_inner(&self) -> &FileDesc {
        &self.0
    }
}

impl IntoInner<FileDesc> for Socket {
    fn into_inner(self) -> FileDesc {
        self.0
    }
}

impl FromInner<FileDesc> for Socket {
    fn from_inner(file_desc: FileDesc) -> Self {
        Self(file_desc)
    }
}

impl AsFd for Socket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for Socket {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for Socket {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

impl FromRawFd for Socket {
    unsafe fn from_raw_fd(raw_fd: RawFd) -> Self {
        Self(FromRawFd::from_raw_fd(raw_fd))
    }
}

// In versions of glibc prior to 2.26, there's a bug where the DNS resolver
// will cache the contents of /etc/resolv.conf, so changes to that file on disk
// can be ignored by a long-running program. That can break DNS lookups on e.g.
// laptops where the network comes and goes. See
// https://sourceware.org/bugzilla/show_bug.cgi?id=984. Note however that some
// distros including Debian have patched glibc to fix this for a long time.
//
// A workaround for this bug is to call the res_init libc function, to clear
// the cached configs. Unfortunately, while we believe glibc's implementation
// of res_init is thread-safe, we know that other implementations are not
// (https://github.com/rust-lang/rust/issues/43592). Code here in libstd could
// try to synchronize its res_init calls with a Mutex, but that wouldn't
// protect programs that call into libc in other ways. So instead of calling
// res_init unconditionally, we call it only when we detect we're linking
// against glibc version < 2.26. (That is, when we both know its needed and
// believe it's thread-safe).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn on_resolver_failure() {
    use crate::sys;

    // If the version fails to parse, we treat it the same as "not glibc".
    if let Some(version) = sys::os::glibc_version() {
        if version < (2, 26) {
            unsafe { libc::res_init() };
        }
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn on_resolver_failure() {}
