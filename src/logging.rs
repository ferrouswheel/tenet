//! Structured logging with timestamps, source locations, and ANSI colour support.
//!
//! Provides the [`tlog!`] macro for consistent log output in the format:
//!
//! ```text
//! 20260211T21:33:12.000 - src/bin/tenet-web.rs:42 - sync: fetched 5 envelope(s)
//! ```
//!
//! When writing to a terminal, output is colour-coded:
//! - Timestamps and source locations are dimmed
//! - Peer IDs and message IDs get consistent colours based on their content
//!
//! By default log lines go to stderr.  Call [`set_writer`] to redirect output
//! to any [`std::io::Write`] implementor (file, in-memory buffer, TUI channel
//! adapter, etc.).  Installing a custom writer also disables ANSI colour codes.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

static COLOUR_ENABLED: AtomicBool = AtomicBool::new(false);

static LOG_WRITER: LazyLock<Mutex<Box<dyn Write + Send>>> =
    LazyLock::new(|| Mutex::new(Box::new(io::stderr())));

/// Initialize the logging system. Call once at startup before any logging.
/// Detects whether stderr supports ANSI colours.
pub fn init() {
    let is_terminal = std::io::stderr().is_terminal();
    COLOUR_ENABLED.store(is_terminal, Ordering::Relaxed);
}

/// Replace the log writer.  All subsequent [`tlog!`] output goes to `w`.
/// Also disables ANSI colour codes, since the new writer is unlikely to be
/// a colour terminal.
pub fn set_writer(w: Box<dyn Write + Send>) {
    COLOUR_ENABLED.store(false, Ordering::Relaxed);
    *LOG_WRITER.lock().unwrap() = w;
}

/// Returns whether ANSI colour output is enabled.
pub fn colour_enabled() -> bool {
    COLOUR_ENABLED.load(Ordering::Relaxed)
}

// ANSI escape codes
const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";

/// Colour palette for ID hashing — bright, visually distinct colours.
const ID_COLOURS: &[&str] = &[
    "\x1b[91m", // bright red
    "\x1b[92m", // bright green
    "\x1b[93m", // bright yellow
    "\x1b[94m", // bright blue
    "\x1b[95m", // bright magenta
    "\x1b[96m", // bright cyan
    "\x1b[31m", // red
    "\x1b[32m", // green
    "\x1b[33m", // yellow
    "\x1b[34m", // blue
    "\x1b[35m", // magenta
    "\x1b[36m", // cyan
];

/// Pick a deterministic colour for the given string.
fn hash_colour(id: &str) -> &'static str {
    let hash: u32 = id
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    ID_COLOURS[(hash as usize) % ID_COLOURS.len()]
}

const LOG_ID_TRUNCATE_LEN: usize = 7;

fn truncate_id(id: &str) -> &str {
    let end = id
        .char_indices()
        .nth(LOG_ID_TRUNCATE_LEN)
        .map(|(i, _)| i)
        .unwrap_or(id.len());
    &id[..end]
}

/// Format a peer ID with consistent colour and truncation.
///
/// Returns e.g. `p-1GDfZU` (plain) or `\x1b[92mp-1GDfZU\x1b[0m` (colour).
pub fn peer_id(id: &str) -> String {
    let short = truncate_id(id);
    if colour_enabled() {
        let colour = hash_colour(id);
        format!("{colour}p-{short}{RESET}")
    } else {
        format!("p-{short}")
    }
}

const MSG_ID_COLOUR: &str = "\x1b[93m"; // bright yellow

/// Format a message ID with consistent colour and truncation.
pub fn msg_id(id: &str) -> String {
    let short = truncate_id(id);
    if colour_enabled() {
        format!("{MSG_ID_COLOUR}m-{short}{RESET}")
    } else {
        format!("m-{short}")
    }
}

/// Format the current wall-clock time as `YYYYMMDDTHH:MM:SS.mmm`.
pub fn format_timestamp() -> String {
    let now = SystemTime::now();
    let duration = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let millis = duration.subsec_millis();

    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Civil date from days since epoch (Howard Hinnant's algorithm).
    let days = (secs / 86400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}{:02}{:02}T{:02}:{:02}:{:02}.{:03}",
        y, m, d, hours, minutes, seconds, millis
    )
}

/// Write a single log line to the current writer.
///
/// Called by the [`tlog!`] macro; not intended for direct use.
pub fn emit(file: &str, line: u32, msg: &str) {
    let ts = format_timestamp();
    let formatted = if colour_enabled() {
        format!("{DIM}{ts}{RESET} {DIM}{file}:{line}{RESET} {msg}")
    } else {
        format!("{ts} - {file}:{line} - {msg}")
    };
    let mut writer = LOG_WRITER.lock().unwrap();
    let _ = writeln!(*writer, "{formatted}");
}

/// Emit a log line to the current writer with timestamp and source location.
///
/// By default writes to stderr.  Install a different destination with
/// [`set_writer`].
///
/// # Usage
///
/// ```ignore
/// tlog!("sync: fetched {} envelope(s)", count);
/// tlog!("sync: peer {} is now online", logging::peer_id(&pid));
/// ```
#[macro_export]
macro_rules! tlog {
    ($($arg:tt)*) => {{
        $crate::logging::emit(file!(), line!(), &format!($($arg)*));
    }};
}
