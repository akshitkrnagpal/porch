//! Helpers shared by the integration test crates.
use std::sync::Mutex;

/// Each test port owns a block of this many ports above it.
const SLOT: u16 = 256;

/// Slot bases: unprivileged, clear of the app ports Porch assigns (4000-4999),
/// and below every OS ephemeral range (Linux starts at 32768; macOS and
/// Windows at 49152). The slot holding Porch's default proxy and discovery
/// ports (7462, 7463) is skipped: a developer's own Porch may be running there,
/// and tests probe the ports just above the default.
fn slots() -> impl Iterator<Item = u16> {
    let defaults = porch::ports::DEFAULT_PROXY_PORT..=porch::ports::DISCOVERY_PORT;
    (1024..4000 - SLOT)
        .step_by(SLOT.into())
        .chain((5120..32768 - SLOT).step_by(SLOT.into()))
        .filter(move |base| !(*base..base + SLOT).any(|p| defaults.contains(&p)))
}

/// A TCP port that is free right now, for a proxy or app the test starts.
///
/// Binding `127.0.0.1:0` and dropping the listener yields an *ephemeral* port.
/// The kernel may hand that port to a concurrent test's `:0` bind, or use it
/// as the source port of any outbound connection, before the process under
/// test binds it. That race made a parallel test's proxy own another test's
/// port (`proxy port N unavailable: it is used by porch`).
///
/// Ports here are never assigned implicitly. Each one is the base of its own
/// 256-port slot, because Porch probes up to 200 ports above a busy proxy port
/// to suggest a free one; adjacent test ports would see those probes as
/// transient conflicts. A slot is claimed with an exclusive lock file held
/// until this process exits, which keeps slots unique among this process's
/// parallel tests and across concurrent test processes (cargo-nextest,
/// several `cargo test` runs).
pub fn free_port() -> u16 {
    use fs2::FileExt;
    // Ports returned by this process, with the lock files that claim them for
    // other processes. Tracked by number too: whether two open files of one
    // process conflict under flock is not something to rely on everywhere.
    static CLAIMS: Mutex<Vec<(u16, std::fs::File)>> = Mutex::new(Vec::new());
    let claims = std::env::temp_dir().join("porch-test-port-slots");
    std::fs::create_dir_all(&claims).unwrap();
    let all: Vec<u16> = slots().collect();
    // Start each process at a different slot so concurrent processes rarely
    // contend for the same lock files.
    let start = std::process::id() as usize % all.len();
    // Concurrent test processes can claim every slot. Their claims end when
    // they exit, and TIME_WAIT on a slot's port expires within minutes, so
    // wait for a slot rather than fail.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        let mut claimed = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
        for port in all[start..].iter().chain(&all[..start]).copied() {
            if claimed.iter().any(|(p, _)| *p == port) {
                continue;
            }
            let Ok(claim) = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(claims.join(port.to_string()))
            else {
                continue;
            };
            if claim.try_lock_exclusive().is_err() {
                continue;
            }
            if bindable(&[port]) {
                claimed.push((port, claim));
                return port;
            }
        }
        drop(claimed);
        assert!(
            std::time::Instant::now() < deadline,
            "every test port slot stayed claimed by a test or busy (TIME_WAIT)"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Whether every one of `ports` can be bound on all IPv4 and IPv6 interfaces
/// right now, like Porch's own reservations (no `SO_REUSEADDR`, so a port held
/// only by TIME_WAIT connections counts as busy).
///
/// On Unix the binds happen in a short-lived child process. A socket this
/// multi-threaded test process opened, even briefly, is copied into every
/// child that a parallel test forks at that moment and stays open there until
/// the child calls exec: the port this function just found free would then
/// refuse the bind of the Porch process or fixture it is handed to.
pub fn bindable(ports: &[u16]) -> bool {
    #[cfg(unix)]
    {
        const CHECK: &str = r#"
import errno, socket, sys
for port in map(int, sys.argv[1:]):
    for family, host in ((socket.AF_INET, '0.0.0.0'), (socket.AF_INET6, '::')):
        try:
            s = socket.socket(family)
        except OSError:
            continue
        with s:
            if family == socket.AF_INET6:
                s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
            try:
                s.bind((host, port))
            except OSError as e:
                if family == socket.AF_INET6 and e.errno in (errno.EADDRNOTAVAIL, errno.EAFNOSUPPORT):
                    continue
                sys.exit(1)
"#;
        std::process::Command::new("python3")
            .arg("-c")
            .arg(CHECK)
            .args(ports.iter().map(u16::to_string))
            .stdin(std::process::Stdio::null())
            .status()
            .expect("port checks require Python 3")
            .success()
    }
    // Windows sockets are not inherited by child processes.
    #[cfg(not(unix))]
    {
        ports
            .iter()
            .all(|port| porch::sockets::reserve(*port).is_ok())
    }
}

/// A TCP listener owned by a child process, for tests that assert what Porch
/// sees once the listener is gone: one in this process could survive its
/// drop, inside a child a parallel test forked at that moment (see
/// [`bindable`]). Dropping it kills the child and reaps it, which closes the
/// listener.
#[cfg(unix)]
#[allow(dead_code)]
pub struct ChildListener {
    child: std::process::Child,
}
#[cfg(unix)]
#[allow(dead_code)]
impl ChildListener {
    /// Listen on `host:port` (port 0 picks one), with `SO_REUSEADDR` when
    /// `reuse` is set, as many real servers do. Returns the bound port.
    pub fn bind(host: &str, port: u16, reuse: bool) -> std::io::Result<(Self, u16)> {
        use std::io::BufRead;
        const LISTEN: &str = r#"
import socket, sys
s = socket.socket()
if sys.argv[3] == '1':
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
try:
    s.bind((sys.argv[1], int(sys.argv[2])))
except OSError as e:
    print('error', e.errno, flush=True)
    sys.exit(1)
s.listen()
print(s.getsockname()[1], flush=True)
sys.stdin.read()
"#;
        let mut child = std::process::Command::new("python3")
            .args([
                "-c",
                LISTEN,
                host,
                &port.to_string(),
                if reuse { "1" } else { "0" },
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line)?;
        let listener = Self { child };
        match line.trim().parse() {
            Ok(port) => Ok((listener, port)),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("cannot listen on {host}:{port}: {line:?}"),
            )),
        }
    }
    /// The process holding the listener.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}
#[cfg(unix)]
impl Drop for ChildListener {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// First port of a slot that Porch may assign to apps. Porch's next-free-port
/// probes reach at most 200 ports above a busy proxy port, so a proxy that
/// moved a few ports up within its slot still never probes this range.
const APP_OFFSET: u16 = 224;

/// `PORCH_APP_PORTS` for a sandbox whose proxy port came from [`free_port`].
///
/// Porch otherwise assigns app ports from the machine-wide 4000-4999 pool.
/// Sandboxes have separate state directories, so nothing stops a parallel
/// test's Porch from assigning the port an app of this test was given but has
/// not bound yet, or ignores (the listen check then finds the other test's
/// app). The unused top of the proxy's own slot is private to this test.
#[allow(dead_code)]
pub fn app_ports(slot: u16) -> String {
    format!("{}-{}", slot + APP_OFFSET, slot + SLOT - 1)
}

/// A loopback port that refuses connections for as long as the returned
/// socket lives: the socket is bound but never listens, so nothing else can
/// bind the port and a connection attempt is reset.
///
/// A test needing a port where nothing listens must not bind `127.0.0.1:0`
/// and drop the listener: the OS hands that port to the next `:0` bind, so the
/// test's probe may reach, and disturb, another test's listener.
#[allow(dead_code)]
pub fn refused_port() -> (socket2::Socket, u16) {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .unwrap();
    socket
        .bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .unwrap();
    let port = socket.local_addr().unwrap().as_socket().unwrap().port();
    (socket, port)
}
