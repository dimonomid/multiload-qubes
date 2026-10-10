//! Shared collector/panel file contract, independent of Xen and GTK/Cairo.
//! Memory and swap are KiB, CPU is whole-machine percent, network is bytes/second.
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_WIRE_BYTES: usize = 32 * 1024;
pub const MAX_SWAP_ENTRIES: usize = 129; // 128 guest readers plus dom0.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapEntry {
    /// Domain incarnation key: a restarted qube starts a new line segment.
    pub id: String,
    /// Validated dom0-sourced qube name (never a guest-provided label).
    pub name: String,
    pub total: u64,
    pub used: u64,
}
impl SwapEntry {
    pub fn percent(&self) -> Option<f64> {
        (self.total > 0).then(|| self.used as f64 / self.total as f64 * 100.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapSnapshot {
    /// Number of running, eligible guests plus dom0, including missing reports.
    pub expected: u32,
    pub entries: Vec<SwapEntry>,
}
impl SwapSnapshot {
    /// None means no reporting domain has enabled swap; never invent a zero.
    pub fn peak(&self) -> Option<f64> {
        self.entries
            .iter()
            .filter_map(SwapEntry::percent)
            .reduce(f64::max)
    }
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
    pub swap: Option<SwapSnapshot>,
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
            Self::InvalidFormat => write!(f, "invalid MQ1/MQ2/MQ3 snapshot format"),
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
        if let Some(swap) = &self.swap {
            let mut ids = std::collections::BTreeSet::new();
            let mut names = std::collections::BTreeSet::new();
            if swap.entries.len() > MAX_SWAP_ENTRIES || swap.entries.len() > swap.expected as usize
            {
                return Err(SnapshotError::InvalidField("swap coverage"));
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
                    || entry.used > entry.total
                    || entry.total > i64::MAX as u64
                    || !ids.insert(&entry.id)
                    || !names.insert(&entry.name)
                {
                    return Err(SnapshotError::InvalidField("swap entry"));
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

    /// Encode MQ3 when swap is present; otherwise preserve the MQ2 layout.
    pub fn encode(&self) -> Result<String, SnapshotError> {
        self.validate()?;
        let cpu = self
            .cpu
            .map_or_else(|| "unknown".into(), |v| format!("{v:.4}"));
        let network = self.network.map_or_else(
            || "unknown unknown".into(),
            |(rx, tx)| format!("{rx:.3} {tx:.3}"),
        );
        let mut line = format!(
            "MQ2 {:.6} {cpu} {} {} {} {} {} {} {network}\n",
            self.timestamp,
            self.total,
            self.allocated,
            self.used,
            self.unknown,
            self.overhead,
            self.period
        );
        if let Some(swap) = &self.swap {
            line.replace_range(..3, "MQ3");
            line.pop(); // Replace the final newline with MQ3's swap extension.
            let entries = if swap.entries.is_empty() {
                "-".to_owned()
            } else {
                swap.entries
                    .iter()
                    .map(|e| format!("{}:{}:{}:{}", e.id, e.name, e.total, e.used))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            line.push_str(&format!(" {} {entries}\n", swap.expected));
        }
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
            || (p.len() == 13 && p[0] == "MQ3"))
        {
            return Err(SnapshotError::InvalidFormat);
        }
        let network = if p[0] == "MQ1" || (p[9] == "unknown" && p[10] == "unknown") {
            None
        } else {
            Some((
                field(p[9], "incoming bytes/second")?,
                field(p[10], "outgoing bytes/second")?,
            ))
        };
        let swap = if p[0] == "MQ3" {
            let mut entries = Vec::new();
            if p[12] != "-" {
                for item in p[12].split(',') {
                    if entries.len() >= MAX_SWAP_ENTRIES {
                        return Err(SnapshotError::InvalidField("swap coverage"));
                    }
                    let fields: Vec<_> = item.split(':').collect();
                    if fields.len() != 4 {
                        return Err(SnapshotError::InvalidField("swap entry"));
                    }
                    entries.push(SwapEntry {
                        id: fields[0].into(),
                        name: fields[1].into(),
                        total: field(fields[2], "swap total KiB")?,
                        used: field(fields[3], "swap used KiB")?,
                    });
                }
            }
            Some(SwapSnapshot {
                expected: field(p[11], "swap expected reporters")?,
                entries,
            })
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
        snapshot.swap = Some(SwapSnapshot {
            expected: 4,
            entries: vec![
                SwapEntry {
                    id: "dom0".into(),
                    name: "dom0".into(),
                    total: 0,
                    used: 0,
                },
                SwapEntry {
                    id: "1/vm-a".into(),
                    name: "work".into(),
                    total: 100,
                    used: 60,
                },
                SwapEntry {
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
        let entries = (0..=MAX_SWAP_ENTRIES)
            .map(|i| format!("id{i}:qube{i}:10:1"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(Snapshot::decode(&format!("{base} 130 {entries}")).is_err());
        assert!(Snapshot::decode(&format!("{base} 1 {}:work:10:1", "a".repeat(97))).is_err());
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
