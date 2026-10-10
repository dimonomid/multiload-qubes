//! Read the bottom physical block-device layer once, excluding partitions and
//! virtual stacks (LVM, dm-crypt, RAID, loop, zram). Kernel sectors are 512 bytes.
use multiload_qubes_snapshot::{disk_name, DiskSnapshot, MAX_DISK_DEVICES};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Identity {
    name: String,
    path: PathBuf,
    device: String,
    sequence: Option<u64>,
}
#[derive(Clone, Copy)]
struct Counters {
    read: u64,
    written: u64,
}
type Frame = BTreeMap<Identity, Counters>;

fn optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}
fn counters(text: &str) -> io::Result<Counters> {
    let fields: Vec<_> = text.split_whitespace().collect();
    if fields.len() < 11 {
        return Err(invalid("incomplete block-device statistics"));
    }
    let sector = |i: usize| {
        fields[i]
            .parse()
            .map_err(|_| invalid("invalid sector counter"))
    };
    Ok(Counters {
        read: sector(2)?,
        written: sector(6)?,
    })
}

fn scan(root: &Path) -> io::Result<Frame> {
    let virtual_root = root
        .parent()
        .ok_or_else(|| invalid("invalid sysfs path"))?
        .join("devices/virtual");
    let mut frame = Frame::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if !disk_name(&name) || path.join("partition").exists() || !path.join("device").exists() {
            continue;
        }
        let real = path.canonicalize()?;
        if real.starts_with(&virtual_root)
            || name.starts_with("sr")
            || name.contains("boot")
            || name.ends_with("rpmb")
            || optional(&path.join("device/type"))?.is_some_and(|s| s.trim() == "5")
            || optional(&path.join("hidden"))?.is_some_and(|s| s.trim() == "1")
        {
            continue;
        }
        // Hardware-backed devices with lower block devices are another stack layer.
        if fs::read_dir(path.join("slaves"))?
            .next()
            .transpose()?
            .is_some()
        {
            continue;
        }
        if frame.len() >= MAX_DISK_DEVICES {
            return Err(invalid("too many physical disks"));
        }
        let device = fs::read_to_string(path.join("dev"))?;
        let sequence = optional(&path.join("diskseq"))?
            .map(|s| {
                s.trim()
                    .parse()
                    .map_err(|_| invalid("invalid disk sequence"))
            })
            .transpose()?;
        let value = counters(&fs::read_to_string(path.join("stat"))?)?;
        // Reading diskseq twice avoids mixing a replacement device with old counters.
        if optional(&path.join("diskseq"))?
            .map(|s| {
                s.trim()
                    .parse::<u64>()
                    .map_err(|_| invalid("invalid disk sequence"))
            })
            .transpose()?
            != sequence
        {
            return Err(invalid("disk changed during sampling"));
        }
        frame.insert(
            Identity {
                name,
                path: real,
                device: device.trim().into(),
                sequence,
            },
            value,
        );
    }
    Ok(frame)
}

#[derive(Default)]
struct Tracker {
    previous: Option<(Duration, Frame)>,
}
impl Tracker {
    fn update(&mut self, frame: Frame, now: Duration) -> DiskSnapshot {
        let devices = frame.keys().map(|id| id.name.clone()).collect();
        let previous = self.previous.replace((now, frame.clone()));
        let rates = (|| {
            let (before, old) = previous?;
            let elapsed = now.checked_sub(before)?.as_secs_f64();
            if elapsed <= 0.0 || frame.is_empty() || !old.keys().eq(frame.keys()) {
                return None;
            }
            let (mut read, mut written) = (0_u128, 0_u128);
            for (id, value) in frame {
                read += value.read.checked_sub(old[&id].read)? as u128;
                written += value.written.checked_sub(old[&id].written)? as u128;
            }
            let (read, written) = (
                read as f64 * 512.0 / elapsed,
                written as f64 * 512.0 / elapsed,
            );
            (read <= 1e15 && written <= 1e15).then_some((read, written))
        })();
        DiskSnapshot { devices, rates }
    }
}

#[derive(Default)]
pub struct Collector {
    tracker: Tracker,
    last_error: Option<String>,
}
impl Collector {
    pub fn sample(&mut self, now: Duration) -> Option<DiskSnapshot> {
        match scan(Path::new("/sys/block")) {
            Ok(frame) => {
                if self.last_error.take().is_some() {
                    eprintln!("Disk metrics recovered");
                }
                Some(self.tracker.update(frame, now))
            }
            Err(error) => {
                self.tracker.previous = None;
                let message = error.to_string();
                if self.last_error.as_ref() != Some(&message) {
                    eprintln!("Disk metrics unavailable: {message}");
                }
                self.last_error = Some(message);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "mq-disks-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("block")).unwrap();
            Self(path)
        }
        fn disk(&self, name: &str, virtual_device: bool, read: u64, write: u64) -> PathBuf {
            let path = self
                .0
                .join(if virtual_device {
                    "devices/virtual"
                } else {
                    "devices/pci"
                })
                .join(name);
            fs::create_dir_all(path.join("device")).unwrap();
            fs::create_dir(path.join("slaves")).unwrap();
            fs::write(path.join("dev"), format!("8:{}", read)).unwrap();
            fs::write(path.join("diskseq"), "1").unwrap();
            fs::write(
                path.join("stat"),
                format!("0 0 {read} 0 0 0 {write} 0 0 0 0"),
            )
            .unwrap();
            symlink(&path, self.0.join("block").join(name)).unwrap();
            path
        }
        fn scan(&self) -> Frame {
            scan(&self.0.join("block")).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn selects_physical_whole_disks_without_counting_storage_layers_twice() {
        let f = Fixture::new();
        f.disk("sda", false, 10, 20);
        f.disk("nvme0n1", false, 30, 40);
        let partition = f.disk("sda1", false, 10, 20);
        fs::write(partition.join("partition"), "1").unwrap();
        for name in ["dm-0", "md0", "loop0", "zram0"] {
            f.disk(name, true, 100, 200);
        }
        let rom = f.disk("sr0", false, 0, 0);
        fs::write(rom.join("device/type"), "5").unwrap();
        let hidden = f.disk("nvme0c0n1", false, 30, 40);
        fs::write(hidden.join("hidden"), "1").unwrap();
        let stacked = f.disk("stack", false, 100, 200);
        fs::write(stacked.join("slaves/sda"), "").unwrap();
        assert_eq!(
            f.scan()
                .keys()
                .map(|id| id.name.as_str())
                .collect::<Vec<_>>(),
            ["nvme0n1", "sda"]
        );
    }

    #[test]
    fn rates_sum_sector_deltas_with_monotonic_time_and_distinguish_idle() {
        let f = Fixture::new();
        let a = f.disk("sda", false, 10, 20);
        let b = f.disk("nvme0n1", false, 30, 40);
        let mut t = Tracker::default();
        assert_eq!(t.update(f.scan(), Duration::ZERO).rates, None);
        fs::write(a.join("stat"), "0 0 14 0 0 0 28 0 0 0 0").unwrap();
        fs::write(b.join("stat"), "0 0 34 0 0 0 44 0 0 0 0 0 0 0 0 0 0").unwrap();
        let s = t.update(f.scan(), Duration::from_secs(2));
        assert_eq!(s.rates, Some((2048.0, 3072.0)));
        assert_eq!(s.devices, ["nvme0n1", "sda"]);
        assert_eq!(
            t.update(f.scan(), Duration::from_secs(3)).rates,
            Some((0.0, 0.0))
        );
        assert_eq!(t.update(f.scan(), Duration::from_secs(3)).rates, None);
    }

    #[test]
    fn hotplug_replacement_resets_and_missing_devices_rebaseline_without_spikes() {
        let f = Fixture::new();
        let a = f.disk("sda", false, 10, 20);
        let mut t = Tracker::default();
        t.update(f.scan(), Duration::ZERO);
        f.disk("sdb", false, 1000000, 2000000);
        assert_eq!(t.update(f.scan(), Duration::from_secs(1)).rates, None);
        assert_eq!(
            t.update(f.scan(), Duration::from_secs(2)).rates,
            Some((0.0, 0.0))
        );
        fs::write(a.join("stat"), "0 0 0 0 0 0 0 0 0 0 0").unwrap();
        assert_eq!(t.update(f.scan(), Duration::from_secs(3)).rates, None);
        fs::write(a.join("diskseq"), "2").unwrap();
        assert_eq!(t.update(f.scan(), Duration::from_secs(4)).rates, None);
        fs::remove_file(f.0.join("block/sdb")).unwrap();
        assert_eq!(t.update(f.scan(), Duration::from_secs(5)).rates, None);
        assert_eq!(t.update(Frame::new(), Duration::from_secs(6)).rates, None);
    }

    #[test]
    fn malformed_or_unreadable_selected_disk_does_not_produce_partial_totals() {
        let f = Fixture::new();
        let a = f.disk("sda", false, 10, 20);
        f.disk("sdb", false, 30, 40);
        fs::write(a.join("stat"), "garbage").unwrap();
        assert!(scan(&f.0.join("block")).is_err());
        fs::remove_file(a.join("stat")).unwrap();
        assert!(scan(&f.0.join("block")).is_err());
        for text in ["", "0 0 -1 0 0 0 1 0 0 0 0", "0 0 1 0 0 0 NaN 0 0 0 0"] {
            assert!(counters(text).is_err());
        }
    }
}
