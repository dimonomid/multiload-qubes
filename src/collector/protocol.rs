use std::collections::BTreeMap;

pub const MEMORY_SCRIPT: &str = include_str!("../../guest/memory.py");
pub const NETWORK_SCRIPT: &str = include_str!("../../guest/network.py");

pub fn number(value: &str) -> Result<u64, String> {
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|c| c.is_ascii_digit()) {
        return Err("invalid unsigned integer".into());
    }
    let n = value.parse::<u64>().map_err(|_| "integer overflow")?;
    if n > i64::MAX as u64 {
        return Err("integer out of bounds".into());
    }
    Ok(n)
}

pub fn qube_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 31
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

pub fn memory_used(total: &str, available: &str) -> Result<u64, String> {
    let total = number(total)?;
    let available = number(available)?;
    if total == 0 || available > total {
        return Err("invalid memory totals".into());
    }
    Ok(total - available)
}

pub fn dom0_used(text: &str) -> Result<u64, String> {
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.split_whitespace().next();
        match key {
            "MemTotal" => total = value,
            "MemAvailable" => available = value,
            _ => {}
        }
    }
    memory_used(
        total.ok_or("missing MemTotal")?,
        available.ok_or("missing MemAvailable")?,
    )
}

/// Swap capacity and occupied slots, in KiB; disabled swap is (0, 0).
pub fn swap_usage(total: &str, free: &str) -> Result<(u64, u64), String> {
    let (total, free) = (number(total)?, number(free)?);
    if free > total {
        return Err("invalid swap totals".into());
    }
    Ok((total, total - free))
}

pub fn dom0_swap(text: &str) -> Result<(u64, u64), String> {
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if let Some(value) = value.split_whitespace().next() {
                fields.insert(key, value);
            }
        }
    }
    swap_usage(
        fields.get("SwapTotal").ok_or("missing SwapTotal")?,
        fields.get("SwapFree").ok_or("missing SwapFree")?,
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reading {
    Memory(u64), // Legacy M1 reporters have no swap measurement.
    MemorySwap { used: u64, total: u64, swapped: u64 },
    Network(f64, f64),
}

#[derive(Clone, Copy)]
pub enum Kind {
    Memory(u32),
    Network,
}

type NetworkFrame = (u64, String, BTreeMap<u64, (u64, u64)>);
pub struct Tracker {
    kind: Kind,
    previous: Option<NetworkFrame>,
}

impl Tracker {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            previous: None,
        }
    }
    pub fn update(&mut self, line: &[u8]) -> Result<Option<Reading>, String> {
        if line.len() > 8192 || !line.is_ascii() {
            return Err("invalid guest frame".into());
        }
        let text = std::str::from_utf8(line).map_err(|_| "invalid encoding")?;
        let fields: Vec<_> = text.split_whitespace().collect();
        if let Kind::Memory(domid) = self.kind {
            if line.len() > 256
                || !((fields.len() == 4 && fields[0] == "M1")
                    || (fields.len() == 6 && fields[0] == "M2"))
                || number(fields[1])? != domid as u64
            {
                return Err("invalid memory frame or changed domain".into());
            }
            let used = memory_used(fields[2], fields[3])?;
            return Ok(Some(if fields[0] == "M2" {
                let (total, swapped) = swap_usage(fields[4], fields[5])?;
                Reading::MemorySwap {
                    used,
                    total,
                    swapped,
                }
            } else {
                Reading::Memory(used)
            }));
        }
        if fields.len() != 4 {
            return Err("invalid guest frame fields".into());
        }
        if fields[0] != "N1"
            || fields[2].len() != 36
            || !fields[2]
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase() || c == b'-')
        {
            return Err("invalid network identity".into());
        }
        let stamp = number(fields[1])?;
        let mut counters = BTreeMap::new();
        if fields[3] != "-" {
            for item in fields[3].split(',') {
                let p: Vec<_> = item.split(':').collect();
                if p.len() != 3 || counters.len() >= 64 {
                    return Err("invalid interface counters".into());
                }
                let (idx, rx, tx) = (number(p[0])?, number(p[1])?, number(p[2])?);
                if idx == 0 || counters.insert(idx, (rx, tx)).is_some() {
                    return Err("duplicate interface".into());
                }
            }
        }
        let current = (stamp, fields[2].to_string(), counters);
        let previous = self.previous.replace(current.clone());
        let Some((before, boot, old)) = previous else {
            return Ok(None);
        };
        let (_, new_boot, counters) = current;
        if counters.is_empty()
            || boot != new_boot
            || !old.keys().eq(counters.keys())
            || stamp <= before
        {
            return Ok(None);
        }
        let elapsed = (stamp - before) as f64 / 1e9;
        if !(0.1..=5.0).contains(&elapsed) {
            return Ok(None);
        }
        let (mut rx, mut tx) = (0.0, 0.0);
        for (idx, (r, t)) in counters {
            let (old_r, old_t) = old[&idx];
            if r < old_r || t < old_t {
                return Ok(None);
            }
            rx += (r - old_r) as f64 / elapsed;
            tx += (t - old_t) as f64 / elapsed;
        }
        if rx > 1e15 || tx > 1e15 {
            return Err("implausible network rates".into());
        }
        Ok(Some(Reading::Network(rx, tx)))
    }
}

pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn network(time: u64, data: &str) -> String {
        format!(
            "N1 {} 12345678-1234-1234-1234-123456789abc {data}",
            time * 1_000_000_000
        )
    }
    #[test]
    fn memory_and_dom0() {
        assert_eq!(memory_used("1000", "400"), Ok(600));
        assert_eq!(
            dom0_used("MemTotal: 1000 kB\nMemAvailable: 400 kB\nSwapTotal: 500 kB\nSwapFree: 0 kB"),
            Ok(600)
        );
        assert!(dom0_used("MemTotal: 1000 kB").is_err());
        assert_eq!(
            Tracker::new(Kind::Memory(23)).update(b"M1 23 1000 400"),
            Ok(Some(Reading::Memory(600)))
        );
        assert_eq!(
            Tracker::new(Kind::Memory(23)).update(b"M1 23 1000 1000"),
            Ok(Some(Reading::Memory(0)))
        );
    }
    #[test]
    fn memory_swap_frames_validate_capacity_and_keep_ram_separate() {
        for (frame, total, swapped) in [
            ("M2 23 1000 400 500 125", 500, 375),
            ("M2 23 1000 400 0 0", 0, 0),
        ] {
            assert_eq!(
                Tracker::new(Kind::Memory(23)).update(frame.as_bytes()),
                Ok(Some(Reading::MemorySwap {
                    used: 600,
                    total,
                    swapped
                }))
            );
        }
        assert_eq!(
            dom0_swap("SwapTotal: 500 kB\nSwapFree: 125 kB"),
            Ok((500, 375))
        );
        assert!(dom0_swap("SwapTotal: 500 kB").is_err());
        for frame in [
            "M2 23 1000 400 0 1",
            "M2 23 1000 400 500 501",
            "M2 23 1000 400 -1 0",
            "M2 23 1000 400 NaN 0",
            "M2 24 1000 400 500 0",
            "M2 23 1000 400 500",
            "M2 23 1000 400 500 0 extra",
            "M1 23 1000 400 500 0",
        ] {
            assert!(
                Tracker::new(Kind::Memory(23))
                    .update(frame.as_bytes())
                    .is_err(),
                "{frame}"
            );
        }
    }

    #[test]
    fn rejects_hostile_memory() {
        for frame in [
            b"M1 24 1000 400".as_slice(),
            b"M1 23 100 101",
            b"M1 23 0 0",
            b"M1 23 -1 0",
            b"M1 23 NaN 0",
            b"\xff",
            b"M1 23 1",
        ] {
            assert!(Tracker::new(Kind::Memory(23)).update(frame).is_err());
        }
        for n in ["", "-1", "1\n", "９", "99999999999999999999"] {
            assert!(number(n).is_err());
        }
        assert!(Tracker::new(Kind::Memory(23)).update(&[b'x'; 257]).is_err());
    }
    #[test]
    fn sums_interfaces_and_distinguishes_idle() {
        let mut t = Tracker::new(Kind::Network);
        assert_eq!(
            t.update(network(1, "2:100:200,3:50:60").as_bytes()),
            Ok(None)
        );
        assert_eq!(
            t.update(network(3, "2:1100:600,3:550:260").as_bytes()),
            Ok(Some(Reading::Network(750.0, 300.0)))
        );
        assert_eq!(
            t.update(network(4, "2:1100:600,3:550:260").as_bytes()),
            Ok(Some(Reading::Network(0.0, 0.0)))
        );
        assert_eq!(t.update(network(5, "-").as_bytes()), Ok(None));
    }
    #[test]
    fn resets_rebaseline() {
        for next in [
            network(2, "2:1:1"),
            network(2, "3:200:200"),
            network(1, "2:100:200"),
            network(20, "2:100:200"),
            network(2, "2:200:200").replace("12345678", "aaaaaaaa"),
        ] {
            let mut t = Tracker::new(Kind::Network);
            t.update(network(1, "2:100:200").as_bytes()).unwrap();
            assert_eq!(t.update(next.as_bytes()), Ok(None));
        }
    }
    #[test]
    fn rejects_network_garbage_and_names() {
        for data in ["2:-1:3", "2:1:2,2:3:4", "0:1:2", "2:1:2:3"] {
            assert!(Tracker::new(Kind::Network)
                .update(network(1, data).as_bytes())
                .is_err());
        }
        assert!(qube_name("sys-net"));
        for name in [
            "",
            "-name",
            "bad;command",
            "bad name",
            "９",
            &"a".repeat(32),
        ] {
            assert!(!qube_name(name));
        }
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
