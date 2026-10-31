//! File formats and metric calculations shared by the collector and parser.
//! This module has no Xen handles or guest command-launching code.
pub mod handoff;
pub mod protocol;
pub mod shutdown;
use multiload_qubes_snapshot::{unix_time_now, DomainUsage, DomainUsageSnapshot, Snapshot};
use protocol::Reading;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Domain {
    pub id: u32,
    pub identity: String,
    pub name: String,
    pub cpu_ns: u64,
    pub vcpus: Option<u32>,
    pub allocated: u64,
    pub paused: bool,
    pub shutdown: bool,
    pub stubdom: Option<u32>,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Host {
    pub total: u64,
    pub free: u64,
    pub cpus: u32,
    pub domains: Vec<Domain>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub id: u32,
    pub uuid: String,
    pub name: String,
}
pub fn eligible(host: &Host) -> BTreeMap<u32, Identity> {
    // Stub domains run device models, not a normal guest reporter. Their RAM
    // still counts toward host allocation, but its used amount stays unknown.
    let stubs: BTreeSet<_> = host.domains.iter().filter_map(|d| d.stubdom).collect();
    host.domains
        .iter()
        .filter(|d| {
            d.id != 0
                && !d.paused
                && !d.shutdown
                && !stubs.contains(&d.id)
                && !d.identity.is_empty()
                && protocol::qube_name(&d.name)
        })
        .map(|d| {
            (
                d.id,
                Identity {
                    id: d.id,
                    uuid: d.identity.clone(),
                    name: d.name.clone(),
                },
            )
        })
        .collect()
}

fn domain_snapshot(
    host: &Host,
    dom0: (u64, u64),
    readings: &BTreeMap<u32, Reading>,
    value: fn(Reading) -> Option<(u64, u64)>,
) -> DomainUsageSnapshot {
    let active = eligible(host);
    let mut entries = vec![DomainUsage {
        id: "dom0".into(),
        name: "dom0".into(),
        total: dom0.0,
        used: dom0.1,
    }];
    for (id, key) in &active {
        if let Some((total, used)) = readings.get(id).copied().and_then(value) {
            entries.push(DomainUsage {
                id: format!("{}/{}", id, key.uuid),
                name: key.name.clone(),
                total,
                used,
            });
        }
    }
    DomainUsageSnapshot {
        expected: active.len() as u32 + 1,
        entries,
    }
}
pub fn swap_snapshot(
    host: &Host,
    dom0: (u64, u64),
    readings: &BTreeMap<u32, Reading>,
) -> DomainUsageSnapshot {
    domain_snapshot(host, dom0, readings, |r| match r {
        Reading::Memory { swap, .. } => swap,
        _ => None,
    })
}
pub fn ram_snapshot(
    host: &Host,
    dom0: (u64, u64),
    readings: &BTreeMap<u32, Reading>,
) -> DomainUsageSnapshot {
    domain_snapshot(host, dom0, readings, |r| match r {
        Reading::Memory { total, used, .. } => Some((total, used)),
        _ => None,
    })
}

pub fn snapshot(
    host: &Host,
    dom0: u64,
    readings: &BTreeMap<u32, u64>,
    cpu: Option<f64>,
    network: Option<Reading>,
    period: f64,
) -> Result<Snapshot, String> {
    let (mut allocated, mut used, mut unknown) = (0_u64, 0_u64, 0_u64);
    for d in &host.domains {
        allocated = allocated
            .checked_add(d.allocated)
            .ok_or("allocation overflow")?;
        let usage = if d.id == 0 {
            Some(dom0)
        } else {
            readings.get(&d.id).copied()
        };
        // Guest reports and Xen allocation are sampled separately. Ballooning
        // can make a report larger than the current allocation.
        if let Some(n) = usage {
            used = used
                .checked_add(n.min(d.allocated))
                .ok_or("usage overflow")?;
        } else {
            unknown = unknown
                .checked_add(d.allocated)
                .ok_or("unknown allocation overflow")?;
        }
    }
    let snapshot = Snapshot {
        timestamp: unix_time_now().map_err(|e| e.to_string())?,
        cpu,
        total: host.total,
        allocated,
        used,
        unknown,
        overhead: host
            .total
            .saturating_sub(allocated)
            .saturating_sub(host.free),
        period,
        swap: None,
        qube_ram: None,
        qube_cpu: None,
        disk: None,
        network: match network {
            Some(Reading::Network(rx, tx)) => Some((rx, tx)),
            _ => None,
        },
    };
    snapshot.validate().map_err(|e| e.to_string())?;
    Ok(snapshot)
}

pub fn publish(path: &Path, text: &str) -> Result<(), String> {
    publish_bytes(path, text.as_bytes())
}

pub fn publish_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let tmp = parent.join(format!(".metrics-{}-{stamp}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        drop(file);
        // The panel must see either the previous complete snapshot or this one.
        // Keeping the temporary file beside the destination allows an atomic rename.
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| e.to_string())
}
