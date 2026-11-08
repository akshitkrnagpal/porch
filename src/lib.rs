pub mod cli;
pub mod config;
pub mod doctor;
pub mod health;
pub mod integrations;
pub mod mcp;
pub mod pem;
pub mod ports;
pub mod process;
pub mod proxy;
pub mod setup;
pub mod shutdown;
pub mod sockets;
pub mod state;
pub mod style;
pub mod tls;
pub mod update;
pub mod upstream;
#[cfg(windows)]
pub mod win_net;

pub mod discovery;
#[cfg(feature = "tray")]
pub mod tray;

// Unit tests share the integration tests' port helpers, which name this crate
// `porch`.
#[cfg(test)]
extern crate self as porch;
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod test_support;
