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

/// Parse `--ip <addr>` and `--port <num>` from `std::env::args()`,
/// returning the resulting `SocketAddr`. Either flag is optional;
/// missing fields fall back to `default`. Invalid values are logged to
/// stderr and ignored (keeps the default for that field).
///
/// Plain hand-rolled parser — wisp doesn't pull in clap for two flags.
/// Server bin uses the result as its bind addr; client bin uses it as
/// the connect target. Same flag spelling on both sides so a user can
/// run `server --ip 0.0.0.0 --port 9000` and `client --ip <server-ip>
/// --port 9000` symmetrically.
pub fn parse_addr_args(default: std::net::SocketAddr) -> std::net::SocketAddr {
    let mut ip = default.ip();
    let mut port = default.port();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--ip" => {
                if let Some(v) = args.get(i + 1) {
                    match v.parse() {
                        Ok(parsed) => ip = parsed,
                        Err(e) => eprintln!("--ip {v:?}: {e}; keeping {ip}"),
                    }
                    i += 2;
                    continue;
                } else {
                    eprintln!("--ip requires a value");
                }
            }
            "--port" => {
                if let Some(v) = args.get(i + 1) {
                    match v.parse() {
                        Ok(parsed) => port = parsed,
                        Err(e) => eprintln!("--port {v:?}: {e}; keeping {port}"),
                    }
                    i += 2;
                    continue;
                } else {
                    eprintln!("--port requires a value");
                }
            }
            _ => {}
        }
        i += 1;
    }
    std::net::SocketAddr::new(ip, port)
}
