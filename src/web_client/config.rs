//! Configuration types and constants for the tenet-web server.

use std::path::PathBuf;

use clap::Parser;

pub(crate) const DEFAULT_TTL_SECONDS: u64 = 3600;
pub(crate) const SYNC_INTERVAL_SECS: u64 = 30;

/// Default look-back window for public mesh queries (2 hours).
pub(crate) const DEFAULT_MESH_WINDOW_SECS: u64 = 7_200;
/// How often to re-query each known peer for new public messages (10 minutes).
pub(crate) const MESH_QUERY_INTERVAL_SECS: u64 = 600;
pub(crate) const WEB_HPKE_INFO: &[u8] = b"tenet-web";
pub(crate) const WEB_PAYLOAD_AAD: &[u8] = b"tenet-web";
pub(crate) const WS_CHANNEL_CAPACITY: usize = 256;
pub(crate) const MAX_WS_CONNECTIONS: usize = 8;
/// Files smaller than this threshold are inlined inside the HPKE-encrypted
/// envelope payload (v1 / `Inline` tier).  Files at or above this size are
/// split into chunks and uploaded to the relay blob endpoint (`RelayBlob` tier).
pub(crate) const INLINE_THRESHOLD: u64 = 256 * 1024; // 256 KiB

/// Target size (bytes) of each plaintext chunk for the `RelayBlob` tier.
pub(crate) const CHUNK_SIZE: usize = 256 * 1024; // 256 KiB plaintext per chunk

/// Maximum total attachment size accepted by `POST /api/attachments`.
/// Raised to accommodate `RelayBlob` tier uploads.
pub(crate) const MAX_ATTACHMENT_SIZE: u64 = 50 * 1024 * 1024; // 50 MiB

/// Web server for the Tenet peer-to-peer social network.
///
/// Serves an embedded SPA, provides REST API + WebSocket for messages,
/// connects to relays, and persists state in SQLite.
///
/// Configuration can be set via CLI arguments or environment variables.
/// CLI arguments take precedence over environment variables.
#[derive(Parser, Debug)]
#[command(name = "tenet-web", version, about)]
pub struct Cli {
    /// HTTP server bind address [env: TENET_WEB_BIND] [default: 127.0.0.1:3000]
    #[arg(long, short = 'b')]
    pub bind: Option<String>,

    /// Data directory for identity and database [env: TENET_HOME] [default: ~/.tenet]
    #[arg(long, short = 'd')]
    pub data_dir: Option<PathBuf>,

    /// Relay server URL for fetching and posting messages [env: TENET_RELAY_URL]
    #[arg(long, short = 'r')]
    pub relay_url: Option<String>,

    /// Identity to use (short ID prefix) [env: TENET_IDENTITY]
    #[arg(long, short = 'i')]
    pub identity: Option<String>,
}

pub struct Config {
    pub bind_addr: String,
    pub data_dir: PathBuf,
    pub relay_url: Option<String>,
    pub identity: Option<String>,
}

impl Config {
    pub fn from_cli_and_env(cli: Cli) -> Self {
        let data_dir = cli
            .data_dir
            .or_else(|| std::env::var("TENET_HOME").ok().map(PathBuf::from))
            .unwrap_or_else(|| {
                std::env::var("HOME")
                    .map(|h| PathBuf::from(h).join(".tenet"))
                    .unwrap_or_else(|_| PathBuf::from(".tenet"))
            });

        let bind_addr = cli
            .bind
            .or_else(|| std::env::var("TENET_WEB_BIND").ok())
            .unwrap_or_else(|| "127.0.0.1:3000".to_string());

        let relay_url = cli
            .relay_url
            .or_else(|| std::env::var("TENET_RELAY_URL").ok());

        let identity = cli
            .identity
            .or_else(|| std::env::var("TENET_IDENTITY").ok());

        Self {
            bind_addr,
            data_dir,
            relay_url,
            identity,
        }
    }
}
