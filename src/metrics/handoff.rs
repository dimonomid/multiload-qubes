//! Root-owned files passed to the parser. Guest bytes never choose a path or identity.
use super::Host;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Read;
use std::path::Path;

pub const MAX_REPORT_BYTES: usize = 8192;
pub const MAX_RAW_BYTES: usize = 64 * 1024;
pub const MAX_HOST_BYTES: usize = 1024 * 1024;
pub const STALE_SECONDS: f64 = 5.0;

#[derive(Serialize, Deserialize)]
pub struct HostFrame {
    pub host: Host,
    pub metrics: String,
    pub memory_enabled: bool,
    pub network_identity: Option<String>,
    pub dom0_ram: (u64, u64),
    pub dom0_swap: (u64, u64),
}

#[derive(Serialize, Deserialize)]
pub struct RawFrame {
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
