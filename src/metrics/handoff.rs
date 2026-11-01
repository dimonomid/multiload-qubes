//! Root-owned files passed to the parser. Guest bytes never choose a path or identity.
use super::{HostSample, ReporterKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

pub const MAX_REPORT_BYTES: usize = 8192;
pub const MAX_RAW_BYTES: usize = 64 * 1024;
pub const MAX_HOST_BYTES: usize = 1024 * 1024;

pub const TICK_WAIT_SECONDS: f64 = 0.1;

#[derive(Serialize, Deserialize)]
pub struct Tick {
    pub tick_id: String,
    pub deadline: f64,
    /// Accepted reporters mapped to their id/UUID strings. The map key alone
    /// cannot distinguish a qube from another qube that later reuses its domain ID.
    pub expected: BTreeMap<ReporterKey, String>,
}

#[derive(Serialize, Deserialize)]
pub struct HostFrame {
    pub tick: Tick,
    pub sample: HostSample,
    pub memory_enabled: bool,
    pub network_identity: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct RawFrame {
    pub tick_id: String,
    pub identity: String,
    pub session: String,
    pub timestamp: f64,
    pub payload: Vec<u8>,
}

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

pub fn fresh(timestamp: f64, now: f64, max_age: f64) -> bool {
    timestamp.is_finite() && now >= timestamp && now - timestamp <= max_age
}
