//! Networking layer. Built on lightyear 0.26 for transport, replication,
//! prediction, and interpolation. The split:
//!
//! - [`protocol`]: Protocol definition — which components replicate, which
//!   are predicted vs. interpolated. Same on every peer.
//! - [`client`]: Client-side plugin (transport setup, prediction state).
//! - [`server`]: Server-side plugin (transport listen, authority).
//! - [`input`]: bei → lightyear input integration.
//! - [`replication`]: Local-only observers that spawn client-side
//!   companions for replicated entities (e.g. portal cameras).
//!
//! The `Cargo.toml` keeps a single lightyear dependency with both `client`
//! and `server` features so each binary picks the right subset.

pub mod client;
pub mod input;
pub mod protocol;
pub mod replication;
pub mod server;

pub use client::ClientNetPlugin;
pub use protocol::ProtocolPlugin;
pub use server::ServerNetPlugin;

/// Default UDP port the server listens on. CLI flags can override.
pub const DEFAULT_PORT: u16 = 5000;

/// Fixed tick duration. Matches avian's substep rate so rollback replays
/// physics deterministically per tick. 60 Hz.
pub const TICK_HZ: u32 = 60;

/// Netcode protocol id. Bumped whenever the wire format changes
/// incompatibly so old clients can't connect to new servers.
pub const PROTOCOL_ID: u64 = 0;

/// Shared netcode private key. Dev/test only — for a real deployment the
/// key would be held by the server and clients would receive a signed
/// `ConnectToken` from a backend auth service.
pub const NETCODE_KEY: [u8; 32] = [0u8; 32];

/// Default address the server binds to and clients connect to.
pub fn default_server_addr() -> std::net::SocketAddr {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT)
}
