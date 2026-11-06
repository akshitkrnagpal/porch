//! Versioned read-only discovery protocol shared by CLI, agents and the tray.
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 1356;
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
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerCatalog {
    pub machine: Machine,
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
