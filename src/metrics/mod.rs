//! File formats and metric calculations shared by the collector and parser.
//! This module has no Xen handles or guest command-launching code.
pub mod handoff;
pub mod protocol;
mod reporter;
pub use reporter::ReporterKey;
pub mod shutdown;
pub use multiload_qubes_snapshot::MAX_NUM_QUBE_REPORTERS;
// Network collection has its own connection, separate from a qube's RAM/swap reporter.
pub const NUM_NETWORK_REPORTERS: usize = 1;
use multiload_qubes_snapshot::{
    DiskSnapshot, DomainCpuSnapshot, DomainUsage, DomainUsageSnapshot, Snapshot,
};
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
    /// XenStore VM identity, without a domain ID prefix.
    pub identity: String,
    pub name: String,
    pub cpu_ns: u64,
    pub vcpus: Option<u32>,
    pub allocated: u64,
    pub paused: bool,
    pub shutdown: bool,
    pub stubdom: Option<u32>,
}
/// Match both fields when associating a report with a qube. Xen can reuse a
/// domain ID for another qube; the ID alone does not establish identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct QubeIdentity {
    pub domain_id: u32,
    /// VM identity from XenStore, commonly /vm/<uuid>.
    pub uuid: String,
}
impl QubeIdentity {
    /// Chart history uses text keys; report matching uses the fields directly.
    pub fn chart_id(&self) -> String {
        format!("{}/{}", self.domain_id, self.uuid)
    }
}
impl Domain {
    pub fn qube_identity(&self) -> QubeIdentity {
        QubeIdentity {
            domain_id: self.id,
            uuid: self.identity.clone(),
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Host {
    pub total: u64,
    pub free: u64,
    pub cpus: u32,
    pub domains: Vec<Domain>,
}

/// Host measurements from one tick, before qube replies are combined with them.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct HostSample {
    pub timestamp: f64,
    pub period: f64,
    pub host: Host,
    pub cpu: Option<f64>,
    pub qube_cpu: Option<DomainCpuSnapshot>,
    pub disk: Option<DiskSnapshot>,
    /// Total and used KiB from dom0's /proc/meminfo.
    pub dom0_ram: (u64, u64),
    pub dom0_swap: (u64, u64),
}
impl HostSample {
    pub fn validate(&self) -> Result<(), String> {
        multiload_qubes_snapshot::validate_sample_header(self.timestamp, self.period, self.cpu)
            .map_err(|e| e.to_string())?;
        if let Some(cpu) = &self.qube_cpu {
            cpu.validate().map_err(|e| e.to_string())?;
        }
        if let Some(disk) = &self.disk {
            disk.validate().map_err(|e| e.to_string())?;
        }
        if self.dom0_ram.0 == 0
            || [self.dom0_ram, self.dom0_swap]
                .into_iter()
                .any(|(total, used)| used > total || total > i64::MAX as u64)
        {
            return Err("invalid dom0 memory usage".into());
        }
        let allocated = self.host.domains.iter().try_fold(0_u64, |sum, domain| {
            sum.checked_add(domain.allocated)
                .ok_or("allocation overflow")
        })?;
        if self.host.total == 0 || allocated > self.host.total {
            return Err("invalid host RAM allocation".into());
        }
        Ok(())
    }
    pub fn snapshot(
        &self,
        readings: &BTreeMap<u32, Reading>,
        network: Option<Reading>,
    ) -> Result<Snapshot, String> {
        let host = &self.host;
        let (mut allocated, mut used, mut unknown) = (0_u64, 0_u64, 0_u64);
        for d in &host.domains {
            allocated = allocated
                .checked_add(d.allocated)
                .ok_or("allocation overflow")?;
            let usage = if d.id == 0 {
                Some(self.dom0_ram.1)
            } else {
                match readings.get(&d.id) {
                    Some(Reading::Memory { used, .. }) => Some(*used),
                    _ => None,
                }
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
            timestamp: self.timestamp,
            cpu: self.cpu,
            total: host.total,
            allocated,
            used,
            unknown,
            overhead: host
                .total
                .saturating_sub(allocated)
                .saturating_sub(host.free),
            period: self.period,
            swap: Some(swap_snapshot(host, self.dom0_swap, readings)),
            qube_ram: Some(ram_snapshot(host, self.dom0_ram, readings)),
            qube_cpu: self.qube_cpu.clone(),
            disk: self.disk.clone(),
            network: match network {
                Some(Reading::Network(rx, tx)) => Some((rx, tx)),
                _ => None,
            },
        };
        snapshot.validate().map_err(|e| e.to_string())?;
        Ok(snapshot)
    }
}

/// Name and identity used to manage a connection to an eligible qube.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Qube {
    pub identity: QubeIdentity,
    pub name: String,
}
pub fn eligible(host: &Host) -> BTreeMap<u32, Qube> {
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
                Qube {
                    identity: d.qube_identity(),
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
    for (id, qube) in &active {
        if let Some((total, used)) = readings.get(id).copied().and_then(value) {
            entries.push(DomainUsage {
                id: qube.identity.chart_id(),
                name: qube.name.clone(),
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
