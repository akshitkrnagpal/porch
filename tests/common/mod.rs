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

/// Slots one test may use (the network test needs eight). A test's first
/// `free_port` claims this many at once (see below); asking for more is a
/// test bug.
const SLOTS_PER_TEST: usize = 8;

/// Ports at the start of a slot that tests bind directly (the proxy port and
/// the few above it that `first_run.rs` occupies). They, and the slot's app
/// port range, must be free when the slot is claimed: a port left in
/// TIME_WAIT by the slot's previous test could become free again in the
/// middle of a test that relies on it being taken, or the other way round.
/// The rest of the slot is only probed by Porch, which skips busy ports.
const CHECKED_PER_SLOT: u16 = 8;

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
/// transient conflicts. A slot is claimed with an exclusive lock file, which
/// keeps slots unique among this process's parallel tests and across
/// concurrent test processes (cargo-nextest, several `cargo test` runs), and
/// the ports tests bind directly must be free when it is claimed.
///
/// Claims belong to the calling test's thread and end when it exits. The
/// first call in a thread claims all `SLOTS_PER_TEST` slots at once, under a
/// lock shared by every test process, and later calls hand them out. A test
/// waiting for slots therefore holds none, and a test holding slots never
/// waits for more, so concurrent tests cannot deadlock on the 117 slots.
pub fn free_port() -> u16 {
    thread_local! {
        static BATCH: std::cell::RefCell<Option<Batch>> = const { std::cell::RefCell::new(None) };
    }
    BATCH.with(|batch| {
        let mut batch = batch.borrow_mut();
        let batch = batch.get_or_insert_with(Batch::claim);
        assert!(
            batch.next < batch.slots.len(),
            "a test may use at most {SLOTS_PER_TEST} ports from free_port()"
        );
        batch.next += 1;
        batch.slots[batch.next - 1].0
    })
}

/// Slot bases this process has claimed, with the lock files that claim them
/// for other processes. Tracked by number too: whether two open files of one
/// process conflict under flock is not something to rely on everywhere.
static CLAIMED: Mutex<Vec<u16>> = Mutex::new(Vec::new());

struct Batch {
    slots: Vec<(u16, std::fs::File)>,
    next: usize,
}
impl Batch {
    fn claim() -> Self {
        use fs2::FileExt;
        let dir = std::env::temp_dir().join("porch-test-port-slots");
        std::fs::create_dir_all(&dir).unwrap();
        let all: Vec<u16> = slots().collect();
        // Start each process at a different slot so concurrent processes
        // rarely contend for the same lock files.
        let start = std::process::id() as usize % all.len();
        // Other tests release slots when they finish; TIME_WAIT on a slot's
        // ports expires within minutes.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        loop {
            let admission = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join("admission"))
                .unwrap();
            admission.lock_exclusive().unwrap();
            let mut claimed = CLAIMED.lock().unwrap_or_else(|e| e.into_inner());
            let mut locked: Vec<(u16, std::fs::File)> = Vec::new();
            let candidates: Vec<u16> = all[start..]
                .iter()
                .chain(&all[..start])
                .copied()
                .filter(|base| !claimed.contains(base))
                .collect();
            let mut candidates = candidates.into_iter();
            // Lock candidates in rounds, keeping only clean ones (a slot's
            // previous test may have left a directly bound port in TIME_WAIT),
            // until enough are held or every slot has been tried.
            loop {
                let want = SLOTS_PER_TEST - locked.len();
                let mut round = Vec::new();
                for base in candidates.by_ref() {
                    let Ok(claim) = std::fs::OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .write(true)
                        .open(dir.join(base.to_string()))
                    else {
                        continue;
                    };
                    if claim.try_lock_exclusive().is_ok() {
                        round.push((base, claim));
                        if round.len() == want {
                            break;
                        }
                    }
                }
                if round.is_empty() {
                    break;
                }
                let bases: Vec<u16> = round.iter().map(|(base, _)| *base).collect();
                let clean = clean_slots(&bases);
                locked.extend(round.into_iter().filter(|(base, _)| clean.contains(base)));
                if locked.len() == SLOTS_PER_TEST {
                    claimed.extend(locked.iter().map(|(base, _)| *base));
                    return Self {
                        slots: locked,
                        next: 0,
                    };
                }
            }
            // All or nothing: release everything and wait holding no slot.
            drop(locked);
            drop(claimed);
            drop(admission);
            assert!(
                std::time::Instant::now() < deadline,
                "not enough free test port slots for ten minutes"
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}
impl Drop for Batch {
    fn drop(&mut self) {
        let mut claimed = CLAIMED.lock().unwrap_or_else(|e| e.into_inner());
        claimed.retain(|base| !self.slots.iter().any(|(b, _)| b == base));
        // Dropping the files releases the locks for other processes.
    }
}

/// The slots among `bases` whose first `CHECKED_PER_SLOT` ports and app
/// port range are bindable (see [`bindable`]).
fn clean_slots(bases: &[u16]) -> Vec<u16> {
    if bases.is_empty() {
        return Vec::new();
    }
    #[cfg(unix)]
    {
        const CHECK: &str = r#"
import errno, socket, sys
size, app, slot = map(int, sys.argv[1:4])
for base in map(int, sys.argv[4:]):
    clean = True
    for port in list(range(base, base + size)) + list(range(base + app, base + slot)):
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
                    clean = False
            if not clean:
                break
        if not clean:
            break
    if clean:
        print(base)
"#;
        let output = std::process::Command::new("python3")
            .arg("-c")
            .arg(CHECK)
            .arg(CHECKED_PER_SLOT.to_string())
            .arg(APP_OFFSET.to_string())
            .arg(SLOT.to_string())
            .args(bases.iter().map(u16::to_string))
            .stdin(std::process::Stdio::null())
            .output()
            .expect("port checks require Python 3");
        assert!(output.status.success(), "slot check failed");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    }
    #[cfg(not(unix))]
    {
        bases
            .iter()
            .copied()
            .filter(|base| {
                (*base..base + CHECKED_PER_SLOT)
                    .chain(base + APP_OFFSET..base + SLOT)
                    .all(|port| porch::sockets::reserve(port).is_ok())
            })
            .collect()
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
#[allow(dead_code)]
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

/// A port that refuses connections for as long as the returned sockets
/// live: they are bound but never listen, so nothing else can bind the port
/// and a connection attempt is reset.
///
/// A test needing a port where nothing listens must not bind `127.0.0.1:0`
/// and drop the listener: the OS hands that port to the next `:0` bind, so the
/// test's probe may reach, and disturb, another test's listener.
///
/// Both families are held (all IPv4 and IPv6 interfaces), so no other process
/// can listen on this port number anywhere and be seen by a port lookup.
#[allow(dead_code)]
pub fn refused_port() -> (RefusedPort, u16) {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
    loop {
        let v4 = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        v4.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())
            .unwrap();
        let port = v4.local_addr().unwrap().as_socket().unwrap().port();
        let v6 = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)).and_then(|v6| {
            v6.set_only_v6(true)?;
            v6.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
            Ok(v6)
        });
        match v6 {
            Ok(v6) => return (RefusedPort(v4, Some(v6)), port),
            // No IPv6 on this machine: nothing can listen there either.
            Err(e)
                if matches!(e.kind(), std::io::ErrorKind::AddrNotAvailable)
                    || e.raw_os_error() == Some(libc_eafnosupport()) =>
            {
                return (RefusedPort(v4, None), port)
            }
            // Someone holds this port's IPv6 side: pick another port.
            Err(_) => continue,
        }
    }
}
/// The sockets holding a [`refused_port`]; dropping them frees the port.
#[allow(dead_code)]
pub struct RefusedPort(socket2::Socket, Option<socket2::Socket>);
#[allow(dead_code)]
fn libc_eafnosupport() -> i32 {
    #[cfg(target_os = "linux")]
    return 97;
    #[cfg(target_os = "macos")]
    return 47;
    #[cfg(windows)]
    return 10047;
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    return -1;
}

/// Create an executable at `path` that this process never holds open for
/// writing.
///
/// A file this multi-threaded process writes is copied, still open for
/// writing, into any child another test forks at that moment, and stays that
/// way until the child execs. Running the file in that window fails with
/// "Text file busy" (ETXTBSY). Only a `cp` child writes the executable here;
/// the source this process writes is never executed.
#[cfg(unix)]
#[allow(dead_code)]
pub fn write_executable(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let source = path.with_file_name(format!(
        ".{}.source-{}-{}",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&source, contents).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_file(path);
    let status = std::process::Command::new("cp")
        .arg(&source)
        .arg(path)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "cp {} {}",
        source.display(),
        path.display()
    );
    std::fs::remove_file(&source).unwrap();
}
