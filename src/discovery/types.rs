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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
/// Unauthenticated probe: is this agent currently showing a pairing code?
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairingStatus {
    pub version: u32,
    pub open: bool,
}
/// Code pairing is mutual: the joining machine sends the credentials the host
/// needs to reach it back, bound to the certificate it observed (TOFU).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairCodeRequest {
    pub version: u32,
    pub code: String,
    /// SHA-256 of the host certificate as seen by the joiner.
    pub host_fingerprint: String,
    pub machine: Machine,
    pub endpoint: String,
    pub fingerprint: String,
    pub token: String,
}
