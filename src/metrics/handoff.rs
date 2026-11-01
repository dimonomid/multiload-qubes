//! Root-owned files passed to the parser. Guest bytes never choose a path or identity.
use super::{HostSample, QubeIdentity, ReporterKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Bound one guest line before storing or parsing it.
pub const MAX_REPORT_BYTES: usize = 8192;
/// JSON wrapper limit, allowing for byte-array encoding of a guest line.
pub const MAX_RAW_BYTES: usize = 64 * 1024;
/// Bound host.json independently of individual guest reports.
pub const MAX_HOST_BYTES: usize = 1024 * 1024;

/// Shared wait for guest replies, starting just before the collector queues requests.
pub const TICK_WAIT_SECONDS: f64 = 0.1;

/// One collector round and the replies the parser should wait for.
#[derive(Serialize, Deserialize)]
pub struct Tick {
    /// Collector-instance and sequence key; never taken from or sent to a guest.
    pub tick_id: String,
    /// Shared reply cutoff as Unix seconds, compared with collector receipt times.
    pub deadline: f64,
    /// Accepted reporters mapped to the qubes seen by the collector. The key alone
    /// cannot distinguish a qube from another qube that later reuses its domain ID.
    pub expected: BTreeMap<ReporterKey, QubeIdentity>,
}

/// Root-owned host.json: host measurements and context for matching guest replies.
#[derive(Serialize, Deserialize)]
pub struct HostFrame {
    /// Requests accepted for this collection round.
    pub tick: Tick,
    /// Host data that does not depend on parsing guest reports.
    pub sample: HostSample,
    /// Whether general qube reporting is enabled; used to prune parser state.
    pub memory_enabled: bool,
    /// Current network source, even if its worker could not accept this tick.
    pub network_identity: Option<QubeIdentity>,
}

/// A bounded guest line wrapped in context supplied by the privileged collector.
#[derive(Serialize, Deserialize)]
pub struct RawFrame {
    /// Request this line answers, even if the reply arrived late.
    pub tick_id: String,
    /// Qube identity fixed when this connection was opened.
    pub identity: QubeIdentity,
    /// Connection key; a new session resets network counter history.
    pub session: String,
    /// Unix seconds when the collector saved the reply, not a guest timestamp.
    pub timestamp: f64,
    /// Unparsed guest bytes without the newline; empty for a disconnect notice.
    pub payload: Vec<u8>,
}

/// Read at most limit + 1 bytes so oversized files can be rejected without loading them.
pub fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err("input file exceeds size limit".into());
    }
    Ok(bytes)
}

/// Reject non-finite, future, and overly old receipt times.
pub fn fresh(timestamp: f64, now: f64, max_age: f64) -> bool {
    timestamp.is_finite() && now >= timestamp && now - timestamp <= max_age
}
