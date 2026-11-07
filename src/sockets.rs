//! Exclusive TCP reservations. Never enable SO_REUSEADDR on live listeners.
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
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
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
}
impl Reservation {
    pub fn port(&self) -> io::Result<u16> {
        Ok(self.v4.local_addr()?.port())
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
    Ok(Reservation { v4, _v6: v6 })
}
