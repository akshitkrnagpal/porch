//! Exclusive TCP reservations.
//!
//! SO_REUSEADDR is never set up front. On Unix it is used only to retry a bind
//! that failed because closed connections still linger in TIME_WAIT, after
//! checking that no process is listening on the port (with it, BSD would let
//! us bind a specific address beside another program's wildcard listener).
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener},
};

fn ipv6_unavailable(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        matches!(
            e.raw_os_error(),
            Some(libc::EAFNOSUPPORT | libc::EADDRNOTAVAIL | libc::EPROTONOSUPPORT)
        )
    }
    #[cfg(not(unix))]
    {
        matches!(e.raw_os_error(), Some(10047 | 10049))
    }
}
pub fn bind(addr: SocketAddr, v6_only: bool) -> io::Result<TcpListener> {
    let domain = if addr.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(v6_only)?;
    }
    #[cfg(windows)]
    {
        listener_overlaps(addr)?;
        exclusive(&socket)?;
    }
    // Windows has no privileged ports: access denied there means the port is
    // held exclusively or reserved by the system, never that elevation helps.
    #[cfg(windows)]
    socket.bind(&addr.into()).map_err(|e| {
        if e.kind() == io::ErrorKind::PermissionDenied {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("port {} is unavailable: {e}", addr.port()),
            )
        } else {
            e
        }
    })?;
    #[cfg(not(windows))]
    if let Err(e) = socket.bind(&addr.into()) {
        if e.kind() != io::ErrorKind::AddrInUse || listening(addr) {
            return Err(e);
        }
        // Only TIME_WAIT connections hold the port (for example right after
        // the proxy restarts): reuse it instead of waiting up to a minute.
        let socket =
            socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
        if addr.is_ipv6() {
            socket.set_only_v6(v6_only)?;
        }
        socket.set_reuse_address(true)?;
        socket.bind(&addr.into())?;
        socket.listen(1024)?;
        return Ok(socket.into());
    }
    socket.listen(1024)?;
    Ok(socket.into())
}
/// Is any process listening on this port (not just lingering connections)?
#[cfg(not(windows))]
fn listening(addr: SocketAddr) -> bool {
    let port = addr.port();
    crate::ports::owner(port).is_some()
        || crate::ports::accepts_on(SocketAddr::from(([127, 0, 0, 1], port)))
        || crate::ports::accepts_on(SocketAddr::from((Ipv6Addr::LOCALHOST, port)))
        || (!addr.ip().is_unspecified() && crate::ports::accepts_on(addr))
}

/// Windows lets a wildcard bind share a port with a specific-address
/// listener, and lets another process bind a more specific address over ours.
/// Exclusive use restores the Unix behavior: a port in use cannot be bound.
#[cfg(windows)]
fn exclusive(socket: &socket2::Socket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{setsockopt, SOL_SOCKET, SO_EXCLUSIVEADDRUSE};
    let on: i32 = 1;
    let status = unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&on as *const i32).cast(),
            std::mem::size_of::<i32>() as i32,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Another process already listening where `addr` would overlap it.
///
/// Windows decides bind conflicts by a mix of SO_REUSEADDR,
/// SO_EXCLUSIVEADDRUSE and dual-stack mode on both sockets, and in several
/// combinations lets a second listener share the port: a loopback bind
/// under another app's wildcard then takes that app's traffic. Unix refuses
/// all of these, so ask the TCP table instead of the bind rules. Overlap
/// follows Unix: a wildcard covers its family (an IPv6 wildcard may be dual
/// stack, so it covers IPv4 too); distinct specific addresses do not overlap.
#[cfg(windows)]
fn listener_overlaps(addr: SocketAddr) -> io::Result<()> {
    let me = std::process::id();
    let overlaps = |ip: IpAddr| {
        if addr.ip().is_unspecified() {
            ip.is_ipv4() == addr.is_ipv4() || (ip.is_ipv6() && ip.is_unspecified())
        } else {
            ip == addr.ip()
                || (ip.is_unspecified() && (ip.is_ipv4() == addr.is_ipv4() || ip.is_ipv6()))
        }
    };
    if addr.port() == 0 {
        return Ok(());
    }
    if let Some((_, _, pid)) = crate::win_net::tcp_listeners()
        .into_iter()
        .find(|(ip, port, pid)| *port == addr.port() && *pid != me && overlaps(*ip))
    {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("port {} is in use by process {pid}", addr.port()),
        ));
    }
    Ok(())
}

/// Hold both address families, including wildcard interfaces, until the app starts.
#[derive(Debug)]
pub struct Reservation {
    v4: TcpListener,
    _v6: Option<TcpListener>,
    closer: Option<Closer>,
}
#[derive(Debug)]
struct Closer {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Reservation {
    pub fn port(&self) -> io::Result<u16> {
        Ok(self.v4.local_addr()?.port())
    }
    /// Accept and close every connection while the port is held.
    ///
    /// A reservation listens (so the port stays exclusively ours), and the
    /// app may start many seconds later, after a sharing tunnel is up. A
    /// client that connects meanwhile, such as the proxy, would wait for a
    /// response that never comes; closing the connection tells it at once
    /// that the app isn't answering yet. (Binding without listening doesn't
    /// do that: macOS drops the connection attempt instead of refusing it.)
    /// Connections are reset, so none lingers to keep the app off the port.
    pub fn close_connections(&mut self) -> io::Result<()> {
        if self.closer.is_some() {
            return Ok(());
        }
        let mut listeners = vec![self.v4.try_clone()?];
        if let Some(v6) = &self._v6 {
            listeners.push(v6.try_clone()?);
        }
        for l in &listeners {
            l.set_nonblocking(true)?;
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("porch-reservation".into())
            .spawn(move || {
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    for l in &listeners {
                        while let Ok((connection, _)) = l.accept() {
                            // Reset rather than close: a closed connection
                            // would linger in TIME_WAIT on this port and
                            // could stop the app from binding it.
                            let _ = socket2::SockRef::from(&connection)
                                .set_linger(Some(std::time::Duration::ZERO));
                            drop(connection);
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })?;
        self.closer = Some(Closer {
            stop,
            thread: Some(thread),
        });
        Ok(())
    }
}
impl Drop for Closer {
    fn drop(&mut self) {
        // Joined, so its copies of the sockets are closed and the port is
        // free once the reservation is dropped.
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
/// Whether `port` could be reserved right now, without accepting connections.
///
/// On Unix the probe binds both families but never listens. A child that
/// another thread forks during the probe keeps a copy of the probe socket
/// until it execs; a listening copy would look like a live listener to
/// [`crate::ports::has_listener`], and a client connecting during the probe
/// would be accepted and then reset. Bind conflicts do not depend on listening
/// for sockets without `SO_REUSEADDR`, so the answer is the same as a
/// reservation's. Windows sockets are never inherited, and Windows decides
/// conflicts with listeners in mind, so the probe there is a reservation.
pub fn probe(port: u16) -> io::Result<()> {
    #[cfg(unix)]
    {
        let socket = |addr: SocketAddr| -> io::Result<socket2::Socket> {
            let domain = if addr.is_ipv6() {
                socket2::Domain::IPV6
            } else {
                socket2::Domain::IPV4
            };
            let socket =
                socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
            if addr.is_ipv6() {
                socket.set_only_v6(true)?;
            }
            socket.bind(&addr.into())?;
            Ok(socket)
        };
        let _v4 = socket(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))?;
        match socket(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)) {
            Ok(_) => Ok(()),
            Err(e) if ipv6_unavailable(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        reserve(port).map(drop)
    }
}
pub fn reserve(port: u16) -> io::Result<Reservation> {
    let v4 = bind(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
        false,
    )?;
    let port = v4.local_addr()?.port();
    let v6 = match bind(
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port),
        true,
    ) {
        Ok(l) => Some(l),
        Err(e) if ipv6_unavailable(&e) => None,
        Err(e) => return Err(e),
    };
    Ok(Reservation {
        v4,
        _v6: v6,
        closer: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn a_reservation_closes_connections_instead_of_leaving_them_waiting() {
        let port = crate::test_support::free_port();
        let mut reservation = reserve(port).unwrap();
        reservation.close_connections().unwrap();
        // A reset may already arrive while connecting; either way the client
        // learns at once that nothing answers.
        let read = std::net::TcpStream::connect(("127.0.0.1", port)).and_then(|mut client| {
            let _ = client.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            client.read(&mut [0u8; 16])
        });
        assert!(
            !matches!(&read, Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)),
            "the connection was left waiting: {read:?}"
        );
        drop(reservation);
        // Released for real: the port can be bound again.
        assert!(crate::test_support::bindable(&[port]));
    }
}
