pub mod cli;
pub mod config;
pub mod doctor;
pub mod health;
pub mod integrations;
pub mod mcp;
pub mod ports;
pub mod process;
pub mod proxy;
pub mod setup;
pub mod shutdown;
pub mod sockets;
pub mod state;
pub mod tls;
pub mod update;
#[cfg(windows)]
pub mod win_net;

pub mod discovery;
#[cfg(feature = "tray")]
pub mod tray;
