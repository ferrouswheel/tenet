//! Configuration types and constants for the tenet-web server.

use std::path::PathBuf;

use clap::Parser;

pub(crate) const DEFAULT_TTL_SECONDS: u64 = 3600;
pub(crate) const SYNC_INTERVAL_SECS: u64 = 30;
pub(crate) const WEB_HPKE_INFO: &[u8] = b"tenet-web";
pub(crate) const WEB_PAYLOAD_AAD: &[u8] = b"tenet-web";
pub(crate) const WS_CHANNEL_CAPACITY: usize = 256;
pub(crate) const MAX_WS_CONNECTIONS: usize = 8;
pub(crate) const MAX_ATTACHMENT_SIZE: u64 = 10 * 1024 * 1024; // 10 MB per attachment

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
