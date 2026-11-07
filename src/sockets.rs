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
        wildcard_free(addr)?;
        exclusive(&socket)?;
    }
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

/// Windows lets a specific address bind succeed while another process holds
/// the wildcard on that port, and then routes that address to us: we would
/// take its traffic. Unix refuses such a bind. Probe the wildcards first;
/// plain (non-exclusive) probes still allow other specific addresses on the
/// port, as Unix does.
///
/// A dual-stack `[::]` listener also receives IPv4 there without holding
/// `0.0.0.0`, so IPv4 addresses probe `[::]` too. A probe cannot tell that
/// listener from an IPv6-only one, so the latter is refused as well: the
/// safe side, since guessing wrong the other way steals traffic.
#[cfg(windows)]
fn wildcard_free(addr: SocketAddr) -> io::Result<()> {
    if addr.ip().is_unspecified() || addr.port() == 0 {
        return Ok(());
    }
    let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), addr.port());
    let v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), addr.port());
    let wildcards: &[SocketAddr] = if addr.is_ipv6() { &[v6] } else { &[v4, v6] };
    for wildcard in wildcards {
        let domain = if wildcard.is_ipv6() {
            socket2::Domain::IPV6
        } else {
            socket2::Domain::IPV4
        };
        let probe =
            socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
        if wildcard.is_ipv6() {
            probe.set_only_v6(true)?;
        }
        match probe.bind(&(*wildcard).into()) {
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "port {} is held on all interfaces by another process",
                        addr.port()
                    ),
                ))
            }
            // Bound, or failed for another reason (no IPv6, say) that the
            // real bind will report if it matters.
            _ => {}
        }
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
