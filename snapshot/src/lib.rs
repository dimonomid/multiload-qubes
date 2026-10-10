//! Shared collector/panel file contract, independent of Xen and GTK/Cairo.
//! Memory and swap are KiB, CPU is whole-machine percent, network is bytes/second.
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_WIRE_BYTES: usize = 64 * 1024;
pub const MAX_DOMAIN_ENTRIES: usize = 129; // 128 guest readers plus dom0.

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
    /// Per-domain swap, absent in legacy snapshots. All quantities are KiB.
    pub swap: Option<DomainUsageSnapshot>,
    /// Physical block-device activity in dom0, absent when unavailable or legacy.
    pub disk: Option<DiskSnapshot>,
    /// Current guest-visible RAM totals and usage, including dom0, in KiB.
    pub qube_ram: Option<DomainUsageSnapshot>,
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
            Self::InvalidFormat => write!(f, "invalid MQ1/MQ2/MQ3/MQ4/MQ5 snapshot format"),
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
        for (usage, is_ram) in [(&self.swap, false), (&self.qube_ram, true)] {
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

    /// Write MQ5 for per-domain RAM, otherwise preserve the MQ2/MQ3/MQ4 layout.
    pub fn encode(&self) -> Result<String, SnapshotError> {
        self.validate()?;
        let cpu = self
            .cpu
            .map_or_else(|| "unknown".into(), |v| format!("{v:.4}"));
        let network = encode_rates(self.network);
        let version = if self.qube_ram.is_some() {
            "MQ5"
        } else if self.disk.is_some() {
            "MQ4"
        } else if self.swap.is_some() {
            "MQ3"
        } else {
            "MQ2"
        };
        let mut line = format!(
            "{version} {:.6} {cpu} {} {} {} {} {} {} {network}",
            self.timestamp,
            self.total,
            self.allocated,
            self.used,
            self.unknown,
            self.overhead,
            self.period
        );
        if version != "MQ2" {
            line.push_str(&format!(" {}", encode_domains(self.swap.as_ref())));
        }
        if let Some(disk) = &self.disk {
            let devices = if disk.devices.is_empty() {
                "-".into()
            } else {
                disk.devices.join(",")
            };
            line.push_str(&format!(" {} {devices}", encode_rates(disk.rates)));
        } else if version == "MQ5" {
            line.push_str(" unknown unknown unknown");
        }
        if let Some(ram) = &self.qube_ram {
            line.push_str(&format!(" {}", encode_domains(Some(ram))));
        }
        line.push('\n');
        if line.len() > MAX_WIRE_BYTES {
            return Err(SnapshotError::TooLarge);
        }
        Ok(line)
    }

    /// Decode all supported versions. Historical data is permitted; readers check freshness.
    pub fn decode(text: &str) -> Result<Self, SnapshotError> {
        if text.len() > MAX_WIRE_BYTES {
            return Err(SnapshotError::TooLarge);
        }
        let p: Vec<_> = text.split_whitespace().collect();
        if !((p.len() == 9 && p[0] == "MQ1")
            || (p.len() == 11 && p[0] == "MQ2")
            || (p.len() == 13 && p[0] == "MQ3")
            || (p.len() == 16 && p[0] == "MQ4")
            || (p.len() == 18 && p[0] == "MQ5"))
        {
            return Err(SnapshotError::InvalidFormat);
        }
        let network = if p[0] == "MQ1" {
            None
        } else {
            decode_rates(p[9], p[10])?
        };
        let swap = if matches!(p[0], "MQ3" | "MQ4" | "MQ5") {
            let value = decode_domains(p[11], p[12])?;
            if p[0] == "MQ3" && value.is_none() {
                return Err(SnapshotError::InvalidField("swap coverage"));
            }
            value
        } else {
            None
        };
        let disk = if matches!(p[0], "MQ4" | "MQ5") {
            let rates = decode_rates(p[13], p[14])?;
            if p[0] == "MQ5" && p[15] == "unknown" {
                if rates.is_some() {
                    return Err(SnapshotError::InvalidField("unknown disk rates"));
                }
                None
            } else {
                Some(DiskSnapshot {
                    rates,
                    devices: if p[15] == "-" {
                        Vec::new()
                    } else {
                        p[15].split(',').map(str::to_owned).collect()
                    },
                })
            }
        } else {
            None
        };
        let qube_ram = if p[0] == "MQ5" {
            Some(decode_domains(p[16], p[17])?.ok_or(SnapshotError::InvalidField("RAM coverage"))?)
        } else {
            None
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
            || now - snapshot.timestamp > (snapshot.period * 4.0).max(5.0)
        {
            return Err(SnapshotError::Stale);
        }
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MQ2: &str = "MQ2 1000.500000 42.5000 32000 24000 10000 2000 1000 1 1024.500 2048.250\n";

    #[test]
    fn mq2_round_trip_preserves_wire_contract() {
        let snapshot = Snapshot::decode(MQ2).unwrap();
        assert_eq!(snapshot.timestamp, 1000.5);
        assert_eq!(snapshot.network, Some((1024.5, 2048.25)));
        assert_eq!(snapshot.encode().unwrap(), MQ2);
        assert_eq!(
            Snapshot::decode(&snapshot.encode().unwrap()).unwrap(),
            snapshot
        );
    }

    #[test]
    fn accepts_legacy_mq1_and_unknown_metrics() {
        let snapshot =
            Snapshot::decode("MQ1 1000.5 unknown 32000 24000 10000 2000 1000 1").unwrap();
        assert_eq!(snapshot.cpu, None);
        assert_eq!(snapshot.network, None);
        assert_eq!(
            snapshot.encode().unwrap(),
            "MQ2 1000.500000 unknown 32000 24000 10000 2000 1000 1 unknown unknown\n"
        );
        assert_eq!(
            Snapshot::decode(&snapshot.encode().unwrap()).unwrap(),
            snapshot
        );
    }

    #[test]
    fn mq3_round_trip_peak_disabled_swap_and_missing_reports() {
        let mut snapshot = Snapshot::decode(MQ2).unwrap();
        let wire = format!(
            "{} 4 dom0:dom0:0:0,1/vm-a:work:100:60,2/vm-b:dev:200:120\n",
            MQ2.trim().replacen("MQ2", "MQ3", 1)
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
    fn mq3_rejects_untrusted_labels_duplicates_and_invalid_totals() {
        let base = MQ2.trim().replacen("MQ2", "MQ3", 1);
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
                Snapshot::decode(&format!("{base} {fields}")).is_err(),
                "{fields}"
            );
        }
        let entries = (0..=MAX_DOMAIN_ENTRIES)
            .map(|i| format!("id{i}:qube{i}:10:1"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(Snapshot::decode(&format!("{base} 130 {entries}")).is_err());
        assert!(Snapshot::decode(&format!("{base} 1 {}:work:10:1", "a".repeat(97))).is_err());
    }

    #[test]
    fn mq4_round_trips_disk_rates_devices_and_unknown_swap() {
        let mut s = Snapshot::decode(MQ2).unwrap();
        s.disk = Some(DiskSnapshot {
            devices: vec!["nvme0n1".into(), "sda".into()],
            rates: Some((1024.5, 2048.25)),
        });
        let wire = format!(
            "{} unknown - 1024.500 2048.250 nvme0n1,sda\n",
            MQ2.trim().replacen("MQ2", "MQ4", 1)
        );
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
        assert!(Snapshot::decode(MQ2).unwrap().disk.is_none());
    }

    #[test]
    fn mq4_rejects_bad_disk_names_duplicates_counts_and_rates() {
        let base = format!("{} unknown -", MQ2.trim().replacen("MQ2", "MQ4", 1));
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
                Snapshot::decode(&format!("{base} {extension}")).is_err(),
                "{extension}"
            );
        }
        let many = (0..=MAX_DISK_DEVICES)
            .map(|i| format!("disk{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(Snapshot::decode(&format!("{base} 0 0 {many}")).is_err());
        assert!(Snapshot::decode(&format!("{base} 0 0 {}", "a".repeat(64))).is_err());
        let mut s = Snapshot::decode(MQ2).unwrap();
        s.disk = Some(DiskSnapshot {
            devices: vec!["sda".into()],
            rates: Some((f64::NAN, 0.0)),
        });
        assert!(s.encode().is_err());
        assert!(Snapshot::decode(
            &format!("{base} 0 0 sda").replace("unknown - 0", "unknown bad 0")
        )
        .is_err());
    }

    #[test]
    fn mq5_preserves_ram_totals_and_optional_legacy_metrics() {
        let mut s = Snapshot::decode(MQ2).unwrap();
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
        assert!(wire.starts_with("MQ5 "));
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
        assert!(Snapshot::decode(MQ2).unwrap().qube_ram.is_none());
    }

    #[test]
    fn mq5_bounds_two_full_domain_groups_and_rejects_duplicate_ram_identities() {
        let mut s = Snapshot::decode(MQ2).unwrap();
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
        s.qube_ram = Some(group);
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
    fn freshness_is_separate_and_deterministic() {
        assert!(Snapshot::decode(MQ2).is_ok());
        assert!(Snapshot::decode_fresh_at(MQ2, 1005.5).is_ok());
        assert_eq!(
            Snapshot::decode_fresh_at(MQ2, 1005.5001),
            Err(SnapshotError::Stale)
        );
        assert!(Snapshot::decode_fresh_at(MQ2, 995.5).is_ok());
        assert_eq!(
            Snapshot::decode_fresh_at(MQ2, 995.4999),
            Err(SnapshotError::Stale)
        );
        let slow = MQ2.replace("1000 1 1024", "1000 10 1024");
        assert!(Snapshot::decode_fresh_at(&slow, 1040.5).is_ok());
        assert_eq!(
            Snapshot::decode_fresh_at(&slow, 1040.5001),
            Err(SnapshotError::Stale)
        );
        assert_eq!(
            Snapshot::decode_fresh_at(MQ2, f64::NAN),
            Err(SnapshotError::Stale)
        );
    }

    #[test]
    fn rejects_wrong_versions_counts_oversize_and_invalid_fields() {
        for text in [
            "",
            "MQ3 1",
            "MQ2 1",
            &format!("{MQ2}extra"),
            &MQ2.replace("24000", "-1"),
        ] {
            assert!(Snapshot::decode(text).is_err(), "accepted {text}");
        }
        assert_eq!(
            Snapshot::decode(&" ".repeat(MAX_WIRE_BYTES + 1)),
            Err(SnapshotError::TooLarge)
        );
        for text in [
            MQ2.replace("1000.500000", "NaN"),
            MQ2.replace("42.5000", "101"),
            MQ2.replace("42.5000", "NaN"),
            MQ2.replace(" 1 1024", " 0 1024"),
            MQ2.replace(" 1 1024", " inf 1024"),
            MQ2.replace("1024.500", "NaN"),
            MQ2.replace("1024.500", "unknown"),
            MQ2.replace("2048.250", "-1"),
            MQ2.replace("2048.250", "1e16"),
        ] {
            assert!(Snapshot::decode(&text).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn encoder_and_decoder_share_accounting_validation() {
        let valid = Snapshot::decode(MQ2).unwrap();
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
            assert!(Snapshot::decode(&format!("MQ1 1000 10 {accounting} 1")).is_err());
        }
    }
}
