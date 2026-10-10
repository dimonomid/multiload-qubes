//! Shared collector/panel file contract, independent of Xen and GTK/Cairo.
//! Memory is KiB, CPU is whole-machine percent, network is bytes/second.
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_WIRE_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// Unix timestamp in seconds; MQ2 writes six fractional digits.
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
            Self::InvalidFormat => write!(f, "invalid MQ1/MQ2 snapshot format"),
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

    /// Encode MQ2 with the existing field order, precision and unknown markers.
    pub fn encode(&self) -> Result<String, SnapshotError> {
        self.validate()?;
        let cpu = self
            .cpu
            .map_or_else(|| "unknown".into(), |v| format!("{v:.4}"));
        let network = self.network.map_or_else(
            || "unknown unknown".into(),
            |(rx, tx)| format!("{rx:.3} {tx:.3}"),
        );
        let line = format!(
            "MQ2 {:.6} {cpu} {} {} {} {} {} {} {network}\n",
            self.timestamp,
            self.total,
            self.allocated,
            self.used,
            self.unknown,
            self.overhead,
            self.period
        );
        if line.len() > MAX_WIRE_BYTES {
            return Err(SnapshotError::TooLarge);
        }
        Ok(line)
    }

    /// Decode both versions. Historical data is permitted; readers check freshness.
    pub fn decode(text: &str) -> Result<Self, SnapshotError> {
        if text.len() > MAX_WIRE_BYTES {
            return Err(SnapshotError::TooLarge);
        }
        let p: Vec<_> = text.split_whitespace().collect();
        if !((p.len() == 9 && p[0] == "MQ1") || (p.len() == 11 && p[0] == "MQ2")) {
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
