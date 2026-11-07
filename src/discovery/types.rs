//! Versioned read-only discovery protocol shared by CLI, agents and the tray.
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
/// Discovery agent port. The app proxy uses 7462; the agent sits beside it.
pub const DEFAULT_PORT: u16 = 7463;
/// A pairing code is valid for five minutes and at most five wrong guesses.
pub const PAIR_CODE_LIFETIME: u64 = 300;
pub const PAIR_CODE_MAX_FAILURES: u32 = 5;
pub const SERVICE_TYPE: &str = "_porch._tcp.local.";
pub const MAX_BODY: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Local,
    Lan,
    Tailscale,
    Explicit,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Healthy,
    Stopped,
    Unknown,
    Unreachable,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Machine {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct App {
    pub id: String,
    pub name: String,
    /// Only usable on the catalog's own machine. Never open this on a peer.
    pub local_url: String,
    pub health: Health,
    /// Explicit agent transport enrollment makes this app available via a local relay.
    pub remote_access: bool,
    pub unavailable_reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Catalog {
    pub version: u32,
    pub machine: Machine,
    pub apps: Vec<App>,
    pub proxy_running: bool,
    pub generated_at: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub machine_id: Option<String>,
    pub name: String,
    /// HTTPS URL with a private/link-local/loopback literal IP, never arbitrary DNS.
    pub endpoint: String,
    pub source: Source,
    pub last_seen: u64,
    /// Tailscale node that reported this address, so every address of one node
    /// can be hidden together once that machine is paired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub machine: Machine,
    pub endpoint: String,
    pub fingerprint: String,
    /// Secret, stored only in private local state. Never serialize into CLI catalogs.
    pub token: String,
    pub sources: Vec<Source>,
    pub last_seen: u64,
    /// Stable, unique hostname label used in `APP.SLUG.localhost` browser URLs.
    #[serde(default)]
    pub slug: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerCatalog {
    pub machine: Machine,
    #[serde(default)]
    pub slug: String,
    pub endpoint: String,
    pub sources: Vec<Source>,
    pub last_seen: u64,
    pub catalog: Option<Catalog>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub local: Catalog,
    pub peers: Vec<PeerCatalog>,
    pub candidates: Vec<Candidate>,
    pub warnings: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
    pub advertise: bool,
    /// Explicitly grants paired peers read-only HTTP/WS access to registered routes.
    pub share_apps: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairRequest {
    pub token: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairResponse {
    pub version: u32,
    pub machine: Machine,
    pub token: String,
}
/// Version of the code-pairing exchange. 1 sent the code in the clear to
/// whichever machine answered; 2 is a SPAKE2 password-authenticated exchange.
pub const PAIRING_VERSION: u32 = 2;
/// Unauthenticated probe: is this agent currently showing a pairing code?
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairingStatus {
    pub version: u32,
    pub open: bool,
    /// Missing (0) on agents that only speak the insecure version 1 exchange.
    #[serde(default)]
    pub pairing: u32,
    /// Public per-window salt mixed into the code before the key exchange.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salt: Option<String>,
}
/// Code pairing, step 1: the joiner's SPAKE2 message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairStart {
    pub pairing: u32,
    pub salt: String,
    pub msg: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairStarted {
    pub session: String,
    pub msg: String,
}
/// Step 2: the joiner proves it derived the key (knows the code) for the host
/// certificate it pinned. The host counts a wrong proof as a wrong code.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairConfirm {
    pub session: String,
    pub confirm: String,
}
/// The host proves the key in return; the joiner checks it before it turns
/// on sharing or mints a credential.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairConfirmed {
    pub confirm: String,
}
/// Step 3: the joiner's identity, agent endpoint, certificate fingerprint and a
/// bearer the host will use to call back, authenticated with the pairing key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairFinish {
    pub session: String,
    pub machine: Machine,
    pub endpoint: String,
    pub fingerprint: String,
    pub token: String,
    pub mac: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairFinished {
    pub version: u32,
    pub machine: Machine,
    pub token: String,
    pub mac: String,
}
/// Lifecycle of one pairing code on the machine showing it. `accepted` means the
/// code was right; `paired` means the new peer has since authenticated.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PairState {
    Waiting,
    Accepted,
    Paired,
    Expired,
    Cancelled,
    Replaced,
}
impl PairState {
    /// The code can no longer be used and nothing further will happen.
    pub fn is_final(self) -> bool {
        !matches!(self, Self::Waiting | Self::Accepted)
    }
}
/// `porch pair status ID --json` (and `pair cancel ID --json`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairStatusReport {
    pub version: u32,
    pub id: String,
    pub state: PairState,
    /// Why a code was cancelled: `user`, `too_many_failures` or `host_full`.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub expires: Option<u64>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub peer: Option<Machine>,
}
/// `porch pair --no-wait --json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairShown {
    pub version: u32,
    pub id: String,
    pub code: String,
    pub expires: u64,
    pub expires_in: u64,
    pub machine: Machine,
    pub endpoint: String,
}
/// A machine that could receive a pairing code, offered when the target is ambiguous.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct JoinCandidate {
    pub name: String,
    pub endpoint: String,
    pub source: Source,
}
/// Why `porch pair CODE` failed. `code` is stable for scripts and the tray:
/// wrong_code, not_waiting, rate_limited, certificate_mismatch, host_full,
/// no_host, multiple_hosts, unreachable, invalid_code, protocol, local.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JoinFailure {
    pub code: String,
    pub message: String,
    /// Trying again with another code or later can succeed.
    pub retry: bool,
    #[serde(default)]
    pub candidates: Vec<JoinCandidate>,
    /// Underlying cause, for logs and terminals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
impl std::fmt::Display for JoinFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(detail) = &self.detail {
            write!(f, " ({detail})")?;
        }
        for c in &self.candidates {
            write!(f, "\n  {} at {}", c.name, c.endpoint.trim_end_matches('/'))?;
        }
        Ok(())
    }
}
impl std::error::Error for JoinFailure {}
/// Error body of `POST /v1/pair-code`. Status codes are unchanged from the
/// plain-text bodies older agents send, so joiners map by status first.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairCodeError {
    pub error: PairCodeErrorBody,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairCodeErrorBody {
    pub code: String,
    pub message: String,
}
