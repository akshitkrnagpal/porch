//! Helpers shared by the integration test crates.
use std::sync::Mutex;

/// Each test port owns a block of this many ports above it.
const SLOT: u16 = 256;

/// Slot bases: unprivileged, clear of the app ports Porch assigns (4000-4999),
/// and below every OS ephemeral range (Linux starts at 32768; macOS and
/// Windows at 49152).
fn slots() -> impl Iterator<Item = u16> {
    (1024..4000 - SLOT)
        .step_by(SLOT.into())
        .chain((5120..32768 - SLOT).step_by(SLOT.into()))
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
    static CLAIMS: Mutex<Vec<std::fs::File>> = Mutex::new(Vec::new());
    let claims = std::env::temp_dir().join("porch-test-port-slots");
    std::fs::create_dir_all(&claims).unwrap();
    let all: Vec<u16> = slots().collect();
    // Start each process at a different slot so concurrent processes rarely
    // contend for the same lock files.
    let start = std::process::id() as usize % all.len();
    let mut claimed = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
    for port in all[start..].iter().chain(&all[..start]).copied() {
        let Ok(claim) = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(claims.join(port.to_string()))
        else {
            continue;
        };
        // flock conflicts between open files even within one process.
        if claim.try_lock_exclusive().is_err() {
            continue;
        }
        // Exclusive wildcard IPv4 and IPv6 binds, like Porch's own reservations.
        if porch::sockets::reserve(port).is_ok() {
            claimed.push(claim);
            return port;
        }
    }
    drop(claimed);
    panic!("every test port slot is claimed by a test or still busy (TIME_WAIT)")
}
