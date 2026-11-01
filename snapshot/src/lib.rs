//! Shared collector/panel file contract, independent of Xen and GTK/Cairo.
//! RAM/swap use KiB; per-domain CPU uses scheduled ns / vCPU budget ns.
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_WIRE_BYTES: usize = 96 * 1024;
/// General qube reporters, shared with the snapshot entry limit below.
pub const MAX_NUM_QUBE_REPORTERS: usize = 128;
// Domain snapshots include dom0; the separate network reporter adds no domain entry.
pub const MAX_DOMAIN_ENTRIES: usize = MAX_NUM_QUBE_REPORTERS + 1;

/// Maximum age in seconds: allow missed ticks, but reject old measurements.
pub fn max_sample_age(period: f64) -> f64 {
    (period * 4.0).max(5.0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DomainUsage {
    /// Domain incarnation key: a restarted qube starts a new line segment.
    pub id: String,
    /// Validated dom0-sourced qube name (never a guest-provided label).
    pub name: String,
    pub total: u64,
    pub used: u64,
}
impl DomainUsage {
    pub fn percent(&self) -> Option<f64> {
        (self.total > 0).then(|| self.used as f64 / self.total as f64 * 100.0)
    }
}

/// Reports may be missing. Keep the expected count separate from the entries
/// so the panel can show partial coverage rather than treating missing qubes as idle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DomainUsageSnapshot {
    /// Number of running, eligible guests plus dom0, including missing reports.
    pub expected: u32,
    pub entries: Vec<DomainUsage>,
}
impl DomainUsageSnapshot {
    /// None means there are no nonzero reporting totals; never invent a zero.
    pub fn peak(&self) -> Option<f64> {
        self.entries
            .iter()
            .filter_map(DomainUsage::percent)
            .reduce(f64::max)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DomainCpuSnapshot {
    /// Sampling window in nanoseconds; entries use scheduled CPU ns / vCPU budget ns.
    pub window_ns: u64,
    pub host_cpus: u32,
    pub usage: DomainUsageSnapshot,
}
impl DomainCpuSnapshot {
    pub fn vcpus(&self, entry: &DomainUsage) -> u64 {
        entry.total / self.window_ns
    }
    pub fn host_percent(&self, entry: &DomainUsage) -> f64 {
        entry.used as f64 / self.window_ns as f64 / self.host_cpus as f64 * 100.0
    }
}

pub const MAX_DISK_DEVICES: usize = 64;

/// Names come from the kernel, and must remain safe in plain-text tooltips/wire records.
pub fn disk_name(name: &str) -> bool {
    name != "unknown"
        && !name.is_empty()
        && name.len() <= 63
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-!.".contains(&b))
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiskSnapshot {
    pub devices: Vec<String>,
    /// Read/write bytes per second. None means warming up or no selected devices.
    pub rates: Option<(f64, f64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// Unix timestamp in seconds; the wire format uses six fractional digits.
    pub timestamp: f64,
    /// Whole-machine CPU percentage, or unavailable/warming up.
    pub cpu: Option<f64>,
    /// Physical host RAM in KiB.
    pub total: u64,
    /// Current Xen allocation in KiB, including unknown usage.
    pub allocated: u64,
    /// Known guest/dom0 used RAM in KiB.
    pub used: u64,
    /// Allocation whose guest usage is unknown, in KiB.
    pub unknown: u64,
    /// Xen/unattributed overhead in KiB, outside domain allocation.
    pub overhead: u64,
    /// Collector sampling interval in seconds.
    pub period: f64,
    /// Incoming and outgoing bytes/second, or unavailable/warming up.
    pub network: Option<(f64, f64)>,
    /// Per-domain swap, or unavailable. All quantities are KiB.
    pub swap: Option<DomainUsageSnapshot>,
    /// Physical block-device activity in dom0, or unavailable.
    pub disk: Option<DiskSnapshot>,
    /// Current guest-visible RAM totals and usage, including dom0, in KiB.
    pub qube_ram: Option<DomainUsageSnapshot>,
    pub qube_cpu: Option<DomainCpuSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    TooLarge,
    InvalidFormat,
    InvalidField(&'static str),
    Stale,
}
impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "snapshot exceeds {MAX_WIRE_BYTES} bytes"),
            Self::InvalidFormat => write!(f, "invalid MQ6 snapshot format"),
            Self::InvalidField(field) => write!(f, "invalid snapshot field: {field}"),
            Self::Stale => write!(f, "snapshot is stale or too far in the future"),
        }
    }
}
impl std::error::Error for SnapshotError {}

pub fn unix_time_now() -> Result<f64, SnapshotError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| t.as_secs_f64())
        .map_err(|_| SnapshotError::InvalidField("system time"))
}

fn field<T: FromStr>(value: &str, name: &'static str) -> Result<T, SnapshotError> {
    value.parse().map_err(|_| SnapshotError::InvalidField(name))
}

fn encode_rates(rates: Option<(f64, f64)>) -> String {
    rates.map_or_else(
        || "unknown unknown".into(),
        |(a, b)| format!("{a:.3} {b:.3}"),
    )
}
fn decode_rates(a: &str, b: &str) -> Result<Option<(f64, f64)>, SnapshotError> {
    if a == "unknown" && b == "unknown" {
        Ok(None)
    } else {
        Ok(Some((
            field(a, "rate bytes/second")?,
            field(b, "rate bytes/second")?,
        )))
    }
}

fn encode_domains(usage: Option<&DomainUsageSnapshot>) -> String {
    let Some(usage) = usage else {
        return "unknown -".into();
    };
    let entries = if usage.entries.is_empty() {
        "-".into()
    } else {
        usage
            .entries
            .iter()
            .map(|e| format!("{}:{}:{}:{}", e.id, e.name, e.total, e.used))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!("{} {entries}", usage.expected)
}
fn decode_domains(
    expected: &str,
    entries: &str,
) -> Result<Option<DomainUsageSnapshot>, SnapshotError> {
    if expected == "unknown" {
        return if entries == "-" {
            Ok(None)
        } else {
            Err(SnapshotError::InvalidField("unknown domain entries"))
        };
    }
    let mut values = Vec::new();
    if entries != "-" {
        for item in entries.split(',') {
            if values.len() >= MAX_DOMAIN_ENTRIES {
                return Err(SnapshotError::InvalidField("domain usage coverage"));
            }
            let fields: Vec<_> = item.split(':').collect();
            if fields.len() != 4 {
                return Err(SnapshotError::InvalidField("domain usage entry"));
            }
            values.push(DomainUsage {
                id: fields[0].into(),
                name: fields[1].into(),
                total: field(fields[2], "domain total KiB")?,
                used: field(fields[3], "domain used KiB")?,
            });
        }
    }
    Ok(Some(DomainUsageSnapshot {
        expected: field(expected, "expected reporters")?,
        entries: values,
    }))
}

impl Snapshot {
    /// Validate the data contract without consulting the clock.
    pub fn validate(&self) -> Result<(), SnapshotError> {
        if !self.timestamp.is_finite() {
            return Err(SnapshotError::InvalidField("timestamp"));
        }
        if !self.period.is_finite() || !(0.1..=60.0).contains(&self.period) {
            return Err(SnapshotError::InvalidField("period (seconds)"));
        }
        if self
            .cpu
            .is_some_and(|v| !v.is_finite() || !(0.0..=100.0).contains(&v))
        {
            return Err(SnapshotError::InvalidField("CPU percent"));
        }
        if let Some((rx, tx)) = self.network {
            if !rx.is_finite()
                || !tx.is_finite()
                || !(0.0..=1e15).contains(&rx)
                || !(0.0..=1e15).contains(&tx)
            {
                return Err(SnapshotError::InvalidField("network bytes/second"));
            }
        }
        if let Some(disk) = &self.disk {
            let names: std::collections::BTreeSet<_> = disk.devices.iter().collect();
            if disk.devices.len() > MAX_DISK_DEVICES
                || names.len() != disk.devices.len()
                || disk.devices.iter().any(|name| !disk_name(name))
                || disk.rates.is_some_and(|(read, write)| {
                    disk.devices.is_empty()
                        || !read.is_finite()
                        || !write.is_finite()
                        || !(0.0..=1e15).contains(&read)
                        || !(0.0..=1e15).contains(&write)
                })
            {
                return Err(SnapshotError::InvalidField("disk activity"));
            }
        }
        if let Some(cpu) = &self.qube_cpu {
            if cpu.window_ns == 0
                || cpu.window_ns > 60_000_000_000
                || cpu.host_cpus == 0
                || cpu.usage.entries.iter().any(|e| {
                    e.total == 0 || e.total % cpu.window_ns != 0 || e.total / cpu.window_ns > 65536
                })
            {
                return Err(SnapshotError::InvalidField(
                    "per-domain CPU window or vCPUs",
                ));
            }
        }
        for (usage, is_ram) in [
            (self.swap.as_ref(), false),
            (self.qube_ram.as_ref(), true),
            (self.qube_cpu.as_ref().map(|c| &c.usage), true),
        ] {
            let Some(swap) = usage else { continue };
            let mut ids = std::collections::BTreeSet::new();
            let mut names = std::collections::BTreeSet::new();
            if swap.entries.len() > MAX_DOMAIN_ENTRIES
                || swap.entries.len() > swap.expected as usize
            {
                return Err(SnapshotError::InvalidField("domain usage coverage"));
            }
            for entry in &swap.entries {
                if entry.id.is_empty()
                    || entry.id.len() > 96
                    || !entry
                        .id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/_-".contains(&b))
                    || entry.name.is_empty()
                    || entry.name.len() > 31
                    || !entry.name.as_bytes()[0].is_ascii_alphabetic()
                    || !entry
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    || (is_ram && entry.total == 0)
                    || entry.used > entry.total
                    || entry.total > i64::MAX as u64
                    || !ids.insert(&entry.id)
                    || !names.insert(&entry.name)
                {
                    return Err(SnapshotError::InvalidField("domain usage entry"));
                }
            }
        }
        // Ordered checks make every subtraction safe even for hostile u64 values.
        if self.total == 0
            || self.allocated > self.total
            || self.unknown > self.allocated
            || self.used > self.allocated - self.unknown
            || self.overhead > self.total - self.allocated
        {
            return Err(SnapshotError::InvalidField("RAM accounting (KiB)"));
        }
        Ok(())
    }

    /// Every metric has a fixed position, including when it is unavailable.
    pub fn encode(&self) -> Result<String, SnapshotError> {
        self.validate()?;
        let cpu = self
            .cpu
            .map_or_else(|| "unknown".into(), |v| format!("{v:.4}"));
        let network = encode_rates(self.network);
        let disk = self.disk.as_ref().map_or_else(
            || "unknown unknown unknown".into(),
            |disk| {
                let devices = if disk.devices.is_empty() {
                    "-".into()
                } else {
                    disk.devices.join(",")
                };
                format!("{} {devices}", encode_rates(disk.rates))
            },
        );
        let qube_cpu = self.qube_cpu.as_ref().map_or_else(
            || "unknown unknown unknown -".into(),
            |cpu| {
                format!(
                    "{} {} {}",
                    cpu.window_ns,
                    cpu.host_cpus,
                    encode_domains(Some(&cpu.usage))
                )
            },
        );
        let line = format!(
            "MQ6 {:.6} {cpu} {} {} {} {} {} {} {network} {} {disk} {} {qube_cpu}\n",
            self.timestamp,
            self.total,
            self.allocated,
            self.used,
            self.unknown,
            self.overhead,
            self.period,
            encode_domains(self.swap.as_ref()),
            encode_domains(self.qube_ram.as_ref()),
        );
        if line.len() > MAX_WIRE_BYTES {
            return Err(SnapshotError::TooLarge);
        }
        Ok(line)
    }

    /// Decode MQ6. Readers check freshness separately.
    pub fn decode(text: &str) -> Result<Self, SnapshotError> {
        if text.len() > MAX_WIRE_BYTES {
            return Err(SnapshotError::TooLarge);
        }
        let p: Vec<_> = text.split_whitespace().collect();
        if p.len() != 22 || p[0] != "MQ6" {
            return Err(SnapshotError::InvalidFormat);
        }
        let network = decode_rates(p[9], p[10])?;
        let swap = decode_domains(p[11], p[12])?;
        let disk_rates = decode_rates(p[13], p[14])?;
        let disk = if p[15] == "unknown" {
            if disk_rates.is_some() {
                return Err(SnapshotError::InvalidField("unknown disk rates"));
            }
            None
        } else {
            Some(DiskSnapshot {
                rates: disk_rates,
                devices: if p[15] == "-" {
                    Vec::new()
                } else {
                    p[15].split(',').map(str::to_owned).collect()
                },
            })
        };
        let qube_ram = decode_domains(p[16], p[17])?;
        let qube_cpu = match (p[18], p[19], decode_domains(p[20], p[21])?) {
            ("unknown", "unknown", None) => None,
            (window, cpus, Some(usage)) => Some(DomainCpuSnapshot {
                window_ns: field(window, "CPU sampling window ns")?,
                host_cpus: field(cpus, "host logical CPU count")?,
                usage,
            }),
            _ => return Err(SnapshotError::InvalidField("CPU coverage")),
        };
        let snapshot = Self {
            timestamp: field(p[1], "timestamp")?,
            cpu: if p[2] == "unknown" {
                None
            } else {
                Some(field(p[2], "CPU percent")?)
            },
            total: field(p[3], "total KiB")?,
            allocated: field(p[4], "allocated KiB")?,
            used: field(p[5], "used KiB")?,
            unknown: field(p[6], "unknown allocation KiB")?,
            overhead: field(p[7], "overhead KiB")?,
            period: field(p[8], "period seconds")?,
            network,
            swap,
            disk,
            qube_ram,
            qube_cpu,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn decode_fresh(text: &str) -> Result<Self, SnapshotError> {
        Self::decode_fresh_at(text, unix_time_now()?)
    }

    /// Same freshness policy as the original panel: five seconds of future clock
    /// tolerance, and max(5 seconds, four collection intervals) of age tolerance.
    pub fn decode_fresh_at(text: &str, now: f64) -> Result<Self, SnapshotError> {
        let snapshot = Self::decode(text)?;
        if !now.is_finite()
            || snapshot.timestamp > now + 5.0
            || now - snapshot.timestamp > max_sample_age(snapshot.period)
        {
            return Err(SnapshotError::Stale);
        }
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MQ6: &str = "MQ6 1000.500000 42.5000 32000 24000 10000 2000 1000 1 1024.500 2048.250 unknown - unknown unknown unknown unknown - unknown unknown unknown -\n";

    fn with_fields(range: std::ops::Range<usize>, fields: &str) -> String {
        let parts: Vec<_> = MQ6.split_whitespace().collect();
        format!(
            "{} {fields} {}\n",
            parts[..range.start].join(" "),
            parts[range.end..].join(" ")
        )
    }

    #[test]
    fn round_trip_preserves_wire_contract() {
        let snapshot = Snapshot::decode(MQ6).unwrap();
        assert_eq!(snapshot.timestamp, 1000.5);
        assert_eq!(snapshot.network, Some((1024.5, 2048.25)));
        assert_eq!(snapshot.encode().unwrap(), MQ6);
        assert_eq!(
            Snapshot::decode(&snapshot.encode().unwrap()).unwrap(),
            snapshot
        );
    }

    #[test]
    fn unavailable_metrics_keep_the_same_layout() {
        let mut snapshot = Snapshot::decode(MQ6).unwrap();
        snapshot.cpu = None;
        snapshot.network = None;
        let wire = snapshot.encode().unwrap();
        assert!(wire.starts_with("MQ6 1000.500000 unknown "));
        assert_eq!(wire.split_whitespace().count(), 22);
        assert_eq!(Snapshot::decode(&wire).unwrap(), snapshot);
        assert!(snapshot.swap.is_none());
        assert!(snapshot.disk.is_none());
        assert!(snapshot.qube_ram.is_none());
        assert!(snapshot.qube_cpu.is_none());
    }

    #[test]
    fn swap_round_trip_peak_disabled_swap_and_missing_reports() {
        let mut snapshot = Snapshot::decode(MQ6).unwrap();
        let wire = with_fields(
            11..13,
            "4 dom0:dom0:0:0,1/vm-a:work:100:60,2/vm-b:dev:200:120",
        );
        snapshot.swap = Some(DomainUsageSnapshot {
            expected: 4,
            entries: vec![
                DomainUsage {
                    id: "dom0".into(),
                    name: "dom0".into(),
                    total: 0,
                    used: 0,
                },
                DomainUsage {
                    id: "1/vm-a".into(),
                    name: "work".into(),
                    total: 100,
                    used: 60,
                },
                DomainUsage {
                    id: "2/vm-b".into(),
                    name: "dev".into(),
                    total: 200,
                    used: 120,
                },
            ],
        });
        assert_eq!(snapshot.swap.as_ref().unwrap().peak(), Some(60.0));
        assert_eq!(snapshot.encode().unwrap(), wire);
        assert_eq!(Snapshot::decode(&wire).unwrap(), snapshot);
        assert!(Snapshot::decode_fresh_at(&wire, 1005.5).is_ok());
        assert_eq!(
            Snapshot::decode_fresh_at(&wire, 1006.0),
            Err(SnapshotError::Stale)
        );
        snapshot.swap.as_mut().unwrap().entries.clear();
        assert_eq!(snapshot.swap.as_ref().unwrap().peak(), None);
        assert_eq!(
            Snapshot::decode(&snapshot.encode().unwrap()).unwrap(),
            snapshot
        );
    }

    #[test]
    fn rejects_untrusted_labels_duplicates_and_invalid_totals() {
        for fields in [
            "0 dom0:dom0:1:0",
            "1 dom0:dom0:0:1",
            "1 dom0:dom0:10:11",
            "1 dom0:bad;name:10:1",
            "1 dom0:bad\u{202e}:10:1",
            "1 bad.id:work:10:1",
            "1 dom0:dom0:-1:0",
            "1 dom0:dom0:10:NaN",
            "2 dom0:dom0:10:1,dom0:dev:10:1",
            "2 a:work:10:1,b:work:10:1",
            "1 dom0:dom0:10",
            "1 dom0:dom0:10:0:extra",
        ] {
            assert!(
                Snapshot::decode(&with_fields(11..13, fields)).is_err(),
                "{fields}"
            );
        }
        let entries = (0..=MAX_DOMAIN_ENTRIES)
            .map(|i| format!("id{i}:qube{i}:10:1"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(Snapshot::decode(&with_fields(
            11..13,
            &format!("{} {entries}", MAX_DOMAIN_ENTRIES + 1)
        ))
        .is_err());
        assert!(Snapshot::decode(&with_fields(
            11..13,
            &format!("1 {}:work:10:1", "a".repeat(97))
        ))
        .is_err());
    }

    #[test]
    fn disk_round_trip_rates_devices_and_unknown_swap() {
        let mut s = Snapshot::decode(MQ6).unwrap();
        s.disk = Some(DiskSnapshot {
            devices: vec!["nvme0n1".into(), "sda".into()],
            rates: Some((1024.5, 2048.25)),
        });
        let wire = with_fields(13..16, "1024.500 2048.250 nvme0n1,sda");
        assert_eq!(s.encode().unwrap(), wire);
        assert_eq!(Snapshot::decode(&wire).unwrap(), s);
        assert!(Snapshot::decode_fresh_at(&wire, 1005.5).is_ok());
        assert_eq!(
            Snapshot::decode_fresh_at(&wire, 1006.0),
            Err(SnapshotError::Stale)
        );
        s.swap = Some(DomainUsageSnapshot {
            expected: 1,
            entries: Vec::new(),
        });
        for rates in [Some((0.0, 0.0)), None] {
            s.disk.as_mut().unwrap().rates = rates;
            assert_eq!(Snapshot::decode(&s.encode().unwrap()).unwrap(), s);
        }
        s.disk.as_mut().unwrap().devices.clear();
        assert_eq!(Snapshot::decode(&s.encode().unwrap()).unwrap(), s);
        assert!(Snapshot::decode(MQ6).unwrap().disk.is_none());
    }

    #[test]
    fn rejects_bad_disk_names_duplicates_counts_and_rates() {
        for extension in [
            "NaN 1 sda",
            "1 inf sda",
            "-1 1 sda",
            "1e16 0 sda",
            "unknown 1 sda",
            "1 unknown sda",
            "0 0 -",
            "0 0 sda,sda",
            "0 0 /dev/sda",
            "0 0 sda;cmd",
            "0 0 sda\u{202e}",
            "0 0 sda,",
            "0 0 sda extra",
        ] {
            assert!(
                Snapshot::decode(&with_fields(13..16, extension)).is_err(),
                "{extension}"
            );
        }
        let many = (0..=MAX_DISK_DEVICES)
            .map(|i| format!("disk{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(Snapshot::decode(&with_fields(13..16, &format!("0 0 {many}"))).is_err());
        assert!(
            Snapshot::decode(&with_fields(13..16, &format!("0 0 {}", "a".repeat(64)))).is_err()
        );
        let mut s = Snapshot::decode(MQ6).unwrap();
        s.disk = Some(DiskSnapshot {
            devices: vec!["sda".into()],
            rates: Some((f64::NAN, 0.0)),
        });
        assert!(s.encode().is_err());
        assert!(Snapshot::decode(&with_fields(11..13, "unknown bad")).is_err());
    }

    #[test]
    fn ram_totals_and_unavailable_metrics_round_trip() {
        let mut s = Snapshot::decode(MQ6).unwrap();
        s.qube_ram = Some(DomainUsageSnapshot {
            expected: 3,
            entries: vec![
                DomainUsage {
                    id: "dom0".into(),
                    name: "dom0".into(),
                    total: 1000,
                    used: 400,
                },
                DomainUsage {
                    id: "1/vm-a".into(),
                    name: "work".into(),
                    total: 2000,
                    used: 1600,
                },
            ],
        });
        let wire = s.encode().unwrap();
        assert!(wire.starts_with("MQ6 "));
        assert!(wire.contains("unknown - unknown unknown unknown 3 dom0:dom0:1000:400"));
        assert_eq!(Snapshot::decode(&wire).unwrap(), s);
        assert_eq!(s.qube_ram.as_ref().unwrap().peak(), Some(80.0));
        assert!(Snapshot::decode_fresh_at(&wire, 1005.5).is_ok());
        assert_eq!(
            Snapshot::decode_fresh_at(&wire, 1006.0),
            Err(SnapshotError::Stale)
        );
        s.disk = Some(DiskSnapshot {
            devices: Vec::new(),
            rates: None,
        });
        s.swap = Some(s.qube_ram.clone().unwrap());
        assert_eq!(Snapshot::decode(&s.encode().unwrap()).unwrap(), s);
        s.qube_ram.as_mut().unwrap().entries[1].total = 1600;
        assert_eq!(s.qube_ram.as_ref().unwrap().peak(), Some(100.0));
        s.qube_ram.as_mut().unwrap().entries[1].total = 0;
        assert!(s.encode().is_err());
        let bad = wire.replace(":1000:400", ":0:0");
        assert!(Snapshot::decode(&bad).is_err());
        let bad = wire.replace("dom0:dom0:1000:400", "dom0:bad;name:1000:400");
        assert!(Snapshot::decode(&bad).is_err());
        let bad = wire.replace(" 3 dom0:", " 1 dom0:");
        assert!(Snapshot::decode(&bad).is_err());
        assert!(Snapshot::decode(MQ6).unwrap().qube_ram.is_none());
    }

    #[test]
    fn bounds_full_domain_groups_and_rejects_duplicate_ram_identities() {
        let mut s = Snapshot::decode(MQ6).unwrap();
        let group = DomainUsageSnapshot {
            expected: MAX_DOMAIN_ENTRIES as u32,
            entries: (0..MAX_DOMAIN_ENTRIES)
                .map(|i| DomainUsage {
                    id: format!("{i}/{}", "a".repeat(80)),
                    name: format!("qube{i}"),
                    total: i64::MAX as u64,
                    used: i64::MAX as u64 / 2,
                })
                .collect(),
        };
        s.swap = Some(group.clone());
        s.qube_ram = Some(group.clone());
        let mut cpu_usage = group;
        for entry in &mut cpu_usage.entries {
            entry.total = 4_000_000_000;
            entry.used = 2_000_000_000;
        }
        s.qube_cpu = Some(DomainCpuSnapshot {
            window_ns: 1_000_000_000,
            host_cpus: 16,
            usage: cpu_usage,
        });
        let wire = s.encode().unwrap();
        assert!(wire.len() > 32 * 1024 && wire.len() < MAX_WIRE_BYTES);
        assert_eq!(Snapshot::decode(&wire).unwrap(), s);
        let bad = wire.replace("1/aaaaaaaa", "0/aaaaaaaa");
        assert!(Snapshot::decode(&bad).is_err());
        let duplicate = s.qube_ram.as_ref().unwrap().entries[0].clone();
        s.qube_ram.as_mut().unwrap().entries.push(duplicate);
        assert!(s.encode().is_err());
    }

    #[test]
    fn cpu_time_budgets_vcpus_and_host_contribution_round_trip() {
        let mut s = Snapshot::decode(MQ6).unwrap();
        s.qube_cpu = Some(DomainCpuSnapshot {
            window_ns: 1_000_000_000,
            host_cpus: 16,
            usage: DomainUsageSnapshot {
                expected: 2,
                entries: vec![DomainUsage {
                    id: "1/vm-a".into(),
                    name: "work".into(),
                    total: 4_000_000_000,
                    used: 2_800_000_000,
                }],
            },
        });
        let wire = s.encode().unwrap();
        assert!(wire.starts_with("MQ6 "));
        assert_eq!(Snapshot::decode(&wire).unwrap(), s);
        let cpu = s.qube_cpu.as_ref().unwrap();
        assert_eq!(cpu.usage.entries[0].percent(), Some(70.0));
        assert_eq!(cpu.vcpus(&cpu.usage.entries[0]), 4);
        assert!((cpu.host_percent(&cpu.usage.entries[0]) - 17.5).abs() < 1e-9);
        assert!(Snapshot::decode_fresh_at(&wire, 1005.5).is_ok());
        assert_eq!(
            Snapshot::decode_fresh_at(&wire, 1006.0),
            Err(SnapshotError::Stale)
        );
        for (before, after) in [
            ("1000000000 16", "0 16"),
            ("1000000000 16", "1000000000 0"),
            ("4000000000:2800000000", "4000000001:2800000000"),
            ("4000000000:2800000000", "4000000000:4000000001"),
        ] {
            assert!(Snapshot::decode(&wire.replace(before, after)).is_err());
        }
        s.qube_cpu.as_mut().unwrap().usage.entries.clear();
        assert_eq!(Snapshot::decode(&s.encode().unwrap()).unwrap(), s);
        s.qube_ram = Some(DomainUsageSnapshot {
            expected: 1,
            entries: vec![DomainUsage {
                id: "dom0".into(),
                name: "dom0".into(),
                total: 1000,
                used: 400,
            }],
        });
        assert_eq!(Snapshot::decode(&s.encode().unwrap()).unwrap(), s);
    }

    #[test]
    fn metric_availability_is_independent_in_one_fixed_layout() {
        let mut full = Snapshot::decode(MQ6).unwrap();
        let usage = DomainUsageSnapshot {
            expected: 1,
            entries: vec![DomainUsage {
                id: "dom0".into(),
                name: "dom0".into(),
                total: 1000,
                used: 400,
            }],
        };
        full.swap = Some(usage.clone());
        full.qube_ram = Some(usage);
        full.disk = Some(DiskSnapshot {
            devices: vec!["sda".into()],
            rates: Some((0.0, 0.0)),
        });
        full.qube_cpu = Some(DomainCpuSnapshot {
            window_ns: 1_000_000_000,
            host_cpus: 4,
            usage: DomainUsageSnapshot {
                expected: 1,
                entries: vec![DomainUsage {
                    id: "dom0".into(),
                    name: "dom0".into(),
                    total: 2_000_000_000,
                    used: 1_000_000_000,
                }],
            },
        });
        // Each metric can be absent without changing the positions of the others.
        for available in 0..64 {
            let mut s = full.clone();
            if available & 1 == 0 {
                s.cpu = None;
            }
            if available & 2 == 0 {
                s.network = None;
            }
            if available & 4 == 0 {
                s.swap = None;
            }
            if available & 8 == 0 {
                s.disk = None;
            }
            if available & 16 == 0 {
                s.qube_ram = None;
            }
            if available & 32 == 0 {
                s.qube_cpu = None;
            }
            let wire = s.encode().unwrap();
            assert!(wire.starts_with("MQ6 "));
            assert_eq!(wire.split_whitespace().count(), 22);
            assert_eq!(
                Snapshot::decode(&wire).unwrap(),
                s,
                "availability mask {available}"
            );
        }
    }

    #[test]
    fn rejects_other_versions_and_partial_unavailable_fields() {
        for version in ["MQ1", "MQ2", "MQ3", "MQ4", "MQ5", "MQ7"] {
            assert_eq!(
                Snapshot::decode(&MQ6.replacen("MQ6", version, 1)),
                Err(SnapshotError::InvalidFormat)
            );
        }
        // Old layouts with their original field counts are rejected too.
        let fields: Vec<_> = MQ6.split_whitespace().collect();
        for (version, count) in [
            ("MQ1", 9),
            ("MQ2", 11),
            ("MQ3", 13),
            ("MQ4", 16),
            ("MQ5", 18),
        ] {
            let wire = format!("{version} {}", fields[1..count].join(" "));
            assert_eq!(Snapshot::decode(&wire), Err(SnapshotError::InvalidFormat));
        }
        for (range, fields) in [
            (9..11, "unknown 0"),
            (11..13, "unknown dom0:dom0:0:0"),
            (13..16, "0 0 unknown"),
            (16..18, "unknown dom0:dom0:1000:400"),
            (18..22, "1000000000 4 unknown -"),
            (18..22, "unknown 4 unknown -"),
            (18..22, "unknown unknown 1 dom0:dom0:1000000000:0"),
        ] {
            assert!(
                Snapshot::decode(&with_fields(range, fields)).is_err(),
                "{fields}"
            );
        }
    }

    #[test]
    fn freshness_is_separate_and_deterministic() {
        assert!(Snapshot::decode(MQ6).is_ok());
        assert!(Snapshot::decode_fresh_at(MQ6, 1005.5).is_ok());
        assert_eq!(
            Snapshot::decode_fresh_at(MQ6, 1005.5001),
            Err(SnapshotError::Stale)
        );
        assert!(Snapshot::decode_fresh_at(MQ6, 995.5).is_ok());
        assert_eq!(
            Snapshot::decode_fresh_at(MQ6, 995.4999),
            Err(SnapshotError::Stale)
        );
        let slow = MQ6.replace("1000 1 1024", "1000 10 1024");
        assert!(Snapshot::decode_fresh_at(&slow, 1040.5).is_ok());
        assert_eq!(
            Snapshot::decode_fresh_at(&slow, 1040.5001),
            Err(SnapshotError::Stale)
        );
        assert_eq!(
            Snapshot::decode_fresh_at(MQ6, f64::NAN),
            Err(SnapshotError::Stale)
        );
    }

    #[test]
    fn rejects_wrong_versions_counts_oversize_and_invalid_fields() {
        for text in [
            "",
            "MQ3 1",
            "MQ6 1",
            &format!("{MQ6}extra"),
            &MQ6.replace("24000", "-1"),
        ] {
            assert!(Snapshot::decode(text).is_err(), "accepted {text}");
        }
        assert_eq!(
            Snapshot::decode(&" ".repeat(MAX_WIRE_BYTES + 1)),
            Err(SnapshotError::TooLarge)
        );
        for text in [
            MQ6.replace("1000.500000", "NaN"),
            MQ6.replace("42.5000", "101"),
            MQ6.replace("42.5000", "NaN"),
            MQ6.replace(" 1 1024", " 0 1024"),
            MQ6.replace(" 1 1024", " inf 1024"),
            MQ6.replace("1024.500", "NaN"),
            MQ6.replace("1024.500", "unknown"),
            MQ6.replace("2048.250", "-1"),
            MQ6.replace("2048.250", "1e16"),
        ] {
            assert!(Snapshot::decode(&text).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn encoder_and_decoder_share_accounting_validation() {
        let valid = Snapshot::decode(MQ6).unwrap();
        let mut invalid = Vec::new();
        let mut s = valid.clone();
        s.total = 0;
        invalid.push(s);
        let mut s = valid.clone();
        s.allocated = u64::MAX;
        invalid.push(s);
        let mut s = valid.clone();
        s.unknown = u64::MAX;
        invalid.push(s);
        let mut s = valid.clone();
        s.used = s.allocated;
        invalid.push(s);
        let mut s = valid.clone();
        s.overhead = s.total;
        invalid.push(s);
        let mut s = valid.clone();
        s.cpu = Some(f64::INFINITY);
        invalid.push(s);
        let mut s = valid.clone();
        s.network = Some((f64::NAN, 0.0));
        invalid.push(s);
        let mut s = valid;
        s.period = -1.0;
        invalid.push(s);
        for s in invalid {
            assert!(s.encode().is_err());
        }
        for accounting in ["0 0 0 0 0", "32 33 0 0 0", "32 24 10 20 0", "32 24 10 0 9"] {
            assert!(Snapshot::decode(&with_fields(3..8, accounting)).is_err());
        }
    }
}
