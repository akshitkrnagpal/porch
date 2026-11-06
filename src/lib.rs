pub mod cli;
pub mod config;
pub mod integrations;
pub mod mcp;
pub mod process;
pub mod proxy;
pub mod sockets;
pub mod state;
pub mod tls;

pub mod discovery;
#[cfg(feature = "tray")]
pub mod tray;
