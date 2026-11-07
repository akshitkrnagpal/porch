//! Windows network facts that Unix gets from getifaddrs and lsof/ss.
#![cfg(windows)]

use std::net::Ipv4Addr;
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_BUFFER_OVERFLOW, ERROR_INSUFFICIENT_BUFFER, NO_ERROR},
    NetworkManagement::{
        IpHelper::{
            GetAdaptersAddresses, GetExtendedTcpTable, GAA_FLAG_SKIP_ANYCAST,
            GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
            MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
        },
        Ndis::IfOperStatusUp,
    },
    Networking::WinSock::{AF_INET, AF_INET6, SOCKADDR_IN},
    System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    },
};

/// Call a size-probing Win32 API until the buffer fits. `usize` storage keeps
/// the returned structures aligned.
fn sized_buffer(mut call: impl FnMut(*mut usize, &mut u32) -> u32) -> Option<Vec<usize>> {
    let mut size = 16 * 1024u32;
    for _ in 0..4 {
        let mut buf = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        match call(buf.as_mut_ptr(), &mut size) {
            NO_ERROR => return Some(buf),
            ERROR_BUFFER_OVERFLOW | ERROR_INSUFFICIENT_BUFFER => continue,
            _ => return None,
        }
    }
    None
}

/// IPv4 addresses of interfaces that are up.
pub fn interface_addresses() -> Vec<Ipv4Addr> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let Some(buf) = sized_buffer(|ptr, size| unsafe {
        GetAdaptersAddresses(
            u32::from(AF_INET),
            flags,
            std::ptr::null(),
            ptr.cast(),
            size,
        )
    }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // SAFETY: the list and every node it links live inside `buf`.
    unsafe {
        let mut adapter = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        while !adapter.is_null() {
            let a = &*adapter;
            if a.OperStatus == IfOperStatusUp {
                let mut unicast = a.FirstUnicastAddress;
                while !unicast.is_null() {
                    let u = &*unicast;
                    let sa = u.Address.lpSockaddr;
                    if !sa.is_null() && (*sa).sa_family == AF_INET {
                        let sin = &*sa.cast::<SOCKADDR_IN>();
                        out.push(Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr)));
                    }
                    unicast = u.Next;
                }
            }
            adapter = a.Next;
        }
    }
    out
}

/// `(port, pid)` for every listening TCP socket, IPv4 and IPv6.
pub fn tcp_listeners() -> Vec<(u16, u32)> {
    let mut out = Vec::new();
    for family in [AF_INET, AF_INET6] {
        let Some(buf) = sized_buffer(|ptr, size| unsafe {
            GetExtendedTcpTable(
                ptr.cast(),
                size,
                0,
                u32::from(family),
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        }) else {
            continue;
        };
        // Both tables are a u32 count followed by rows at the row type's alignment.
        // SAFETY: the API wrote `count` rows into `buf`.
        unsafe {
            let count = *buf.as_ptr().cast::<u32>() as usize;
            if family == AF_INET {
                let rows = rows::<MIB_TCPROW_OWNER_PID>(&buf, count);
                out.extend(rows.iter().map(|r| (port(r.dwLocalPort), r.dwOwningPid)));
            } else {
                let rows = rows::<MIB_TCP6ROW_OWNER_PID>(&buf, count);
                out.extend(rows.iter().map(|r| (port(r.dwLocalPort), r.dwOwningPid)));
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// # Safety
/// `buf` must hold a table whose header is a u32 count followed by `count` rows of `T`.
unsafe fn rows<T>(buf: &[usize], count: usize) -> &[T] {
    let offset = std::mem::size_of::<u32>().next_multiple_of(std::mem::align_of::<T>());
    let available = std::mem::size_of_val(buf).saturating_sub(offset) / std::mem::size_of::<T>();
    let first = buf.as_ptr().cast::<u8>().add(offset).cast::<T>();
    std::slice::from_raw_parts(first, count.min(available))
}

/// The port is in network byte order in the low 16 bits.
fn port(raw: u32) -> u16 {
    u16::from_be(raw as u16)
}

/// Executable file name of a process, such as `node.exe`.
pub fn process_name(pid: u32) -> Option<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
    };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    Some(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_owned())
}
