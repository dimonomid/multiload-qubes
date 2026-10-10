mod diagnostics;
pub mod protocol;
mod shutdown;
pub mod stream;
pub mod xen;

use multiload_qubes_snapshot::{unix_time_now, Snapshot, SwapEntry, SwapSnapshot};
use protocol::{Kind, Reading};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use stream::{CommandSpec, Reader, Timing};

#[derive(Clone, Debug)]
pub struct Domain {
    pub id: u32,
    pub identity: String,
    pub name: String,
    pub cpu_ns: u64,
    pub allocated: u64,
    pub paused: bool,
    pub shutdown: bool,
    pub stubdom: Option<u32>,
}
#[derive(Clone)]
pub struct Host {
    pub total: u64,
    pub free: u64,
    pub cpus: u32,
    pub domains: Vec<Domain>,
}

#[derive(Default)]
pub struct CpuTracker {
    previous: BTreeMap<(u32, String), u64>,
    time: Option<Duration>,
}
impl CpuTracker {
    pub fn update(&mut self, host: &Host, now: Duration) -> Option<f64> {
        let mut delta = 0.0;
        let mut current = BTreeMap::new();
        for domain in &host.domains {
            let key = (domain.id, domain.identity.clone());
            if let Some(before) = self.previous.get(&key) {
                delta += domain.cpu_ns.saturating_sub(*before) as f64;
            }
            current.insert(key, domain.cpu_ns);
        }
        let elapsed = self
            .time
            .and_then(|t| now.checked_sub(t))
            .map(|t| t.as_secs_f64());
        self.previous = current;
        self.time = Some(now);
        match elapsed {
            Some(elapsed) if elapsed > 0.0 && host.cpus > 0 => {
                Some((delta / 1e9 / elapsed / host.cpus as f64 * 100.0).clamp(0.0, 100.0))
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    id: u32,
    uuid: String,
    name: String,
}
fn eligible(host: &Host) -> BTreeMap<u32, Identity> {
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

pub struct MemoryPool {
    readers: BTreeMap<u32, (Identity, Reader)>,
    retired: Vec<Reader>,
    limit: usize,
    factory: fn(&Domain) -> std::io::Result<Reader>,
}
impl MemoryPool {
    pub fn new() -> Self {
        Self {
            readers: BTreeMap::new(),
            retired: Vec::new(),
            limit: 128,
            factory: |d| {
                Reader::spawn(
                    CommandSpec::qube(&d.name, Kind::Memory(d.id)),
                    Kind::Memory(d.id),
                    Timing::default(),
                )
            },
        }
    }
    pub fn sync(&mut self, host: &Host) {
        let active = eligible(host);
        let remove: Vec<_> = self
            .readers
            .iter()
            .filter(|(id, (key, _))| active.get(id) != Some(key))
            .map(|(id, _)| *id)
            .collect();
        for id in remove {
            if let Some((_, reader)) = self.readers.remove(&id) {
                reader.stop();
                self.retired.push(reader);
            }
        }
        self.retired.retain(|r| !r.finished());
        for domain in &host.domains {
            if self.readers.len() + self.retired.len() >= self.limit {
                break;
            }
            let Some(key) = active.get(&domain.id) else {
                continue;
            };
            if let std::collections::btree_map::Entry::Vacant(entry) = self.readers.entry(domain.id)
            {
                match (self.factory)(domain) {
                    Ok(reader) => {
                        entry.insert((key.clone(), reader));
                    }
                    Err(error) => eprintln!("Cannot create memory reader: {error}"),
                }
            }
        }
    }
    pub fn readings(&self, host: &Host) -> BTreeMap<u32, u64> {
        host.domains
            .iter()
            .filter_map(|d| {
                let (identity, reader) = self.readers.get(&d.id)?;
                if identity.uuid != d.identity || identity.name != d.name {
                    return None;
                }
                match reader.value() {
                    Some(Reading::Memory(used) | Reading::MemorySwap { used, .. }) => {
                        Some((d.id, used))
                    }
                    _ => None,
                }
            })
            .collect()
    }
}
/// Missing reports stay absent, and therefore cannot be mistaken for empty swap.
fn swap_snapshot(host: &Host, dom0: (u64, u64), pool: Option<&MemoryPool>) -> SwapSnapshot {
    let active = eligible(host);
    let mut entries = vec![SwapEntry {
        id: "dom0".into(),
        name: "dom0".into(),
        total: dom0.0,
        used: dom0.1,
    }];
    if let Some(pool) = pool {
        for (id, key) in &active {
            let Some((identity, reader)) = pool.readers.get(id) else {
                continue;
            };
            if identity != key {
                continue;
            }
            if let Some(Reading::MemorySwap { total, swapped, .. }) = reader.value() {
                entries.push(SwapEntry {
                    id: format!("{}/{}", id, key.uuid),
                    name: key.name.clone(),
                    total,
                    used: swapped,
                });
            }
        }
    }
    SwapSnapshot {
        expected: active.len() as u32 + 1,
        entries,
    }
}

impl Drop for MemoryPool {
    fn drop(&mut self) {
        // Stop every worker first, then their Drop implementations join concurrently exiting threads.
        for (_, reader) in self.readers.values() {
            reader.stop();
        }
        for reader in &self.retired {
            reader.stop();
        }
    }
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
        network: match network {
            Some(Reading::Network(rx, tx)) => Some((rx, tx)),
            _ => None,
        },
    };
    snapshot.validate().map_err(|e| e.to_string())?;
    Ok(snapshot)
}

pub fn publish(path: &Path, text: &str) -> Result<(), String> {
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
        file.write_all(text.as_bytes())?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| e.to_string())
}

pub struct Options {
    pub once: bool,
    pub interval: f64,
    pub output: std::path::PathBuf,
    pub network: Option<String>,
    pub memory: bool,
}
pub fn run(options: Options) -> Result<(), String> {
    let mut xen = xen::Xen::open()?;
    let mut cpu = CpuTracker::default();
    let start = Instant::now();
    if options.once {
        let host = xen.sample()?;
        let meminfo = fs::read_to_string("/proc/meminfo").map_err(|e| e.to_string())?;
        let usage = protocol::dom0_used(&meminfo)?;
        let mut data = snapshot(&host, usage, &BTreeMap::new(), None, None, options.interval)?;
        data.swap = Some(swap_snapshot(&host, protocol::dom0_swap(&meminfo)?, None));
        print!("{}", data.encode().map_err(|e| e.to_string())?);
        return Ok(());
    }
    let parent = options
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let shutdown = shutdown::Shutdown::register()
        .map_err(|e| format!("Cannot install signal handlers: {e}"))?;
    let network = options
        .network
        .map(|name| {
            Reader::spawn(
                CommandSpec::qube(&name, Kind::Network),
                Kind::Network,
                Timing::default(),
            )
        })
        .transpose()
        .map_err(|e| e.to_string())?;
    let mut memory = options.memory.then(MemoryPool::new);
    while !shutdown.requested() {
        let tick = Instant::now();
        let result = (|| {
            let host = xen.sample()?;
            let cpu = cpu.update(&host, start.elapsed());
            let readings = if let Some(pool) = &mut memory {
                pool.sync(&host);
                pool.readings(&host)
            } else {
                BTreeMap::new()
            };
            let meminfo = fs::read_to_string("/proc/meminfo").map_err(|e| e.to_string())?;
            let dom0 = protocol::dom0_used(&meminfo)?;
            let mut data = snapshot(
                &host,
                dom0,
                &readings,
                cpu,
                network.as_ref().and_then(Reader::value),
                options.interval,
            )?;
            data.swap = Some(swap_snapshot(
                &host,
                protocol::dom0_swap(&meminfo)?,
                memory.as_ref(),
            ));
            publish(&options.output, &data.encode().map_err(|e| e.to_string())?)
        })();
        if let Err(error) = result {
            eprintln!("Cannot collect metrics: {error}");
        }
        while !shutdown.requested() && tick.elapsed().as_secs_f64() < options.interval {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    if let Some(reader) = &network {
        reader.stop();
    }
    drop(memory);
    drop(network);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn domain(id: u32) -> Domain {
        Domain {
            id,
            identity: format!("uuid-{id}"),
            name: format!("qube-{id}"),
            cpu_ns: 0,
            allocated: 1000,
            paused: false,
            shutdown: false,
            stubdom: None,
        }
    }
    fn host() -> Host {
        Host {
            total: 4000,
            free: 900,
            cpus: 4,
            domains: vec![domain(0), domain(1), domain(2)],
        }
    }
    #[test]
    fn cpu_normalization_reset_new_domains_and_shutdown() {
        let mut h = host();
        let mut cpu = CpuTracker::default();
        assert_eq!(cpu.update(&h, Duration::from_secs(1)), None);
        h.domains[0].cpu_ns = 1_000_000_000;
        h.domains[1].cpu_ns = 1_000_000_000;
        assert_eq!(cpu.update(&h, Duration::from_secs(2)), Some(50.0));
        h.domains[0].cpu_ns = 0;
        h.domains[1].identity = "new-uuid".into();
        h.domains[1].cpu_ns = 9_000_000_000;
        assert_eq!(cpu.update(&h, Duration::from_secs(3)), Some(0.0));
        h.domains.pop();
        h.domains[0].cpu_ns = 1_000_000_000;
        assert_eq!(cpu.update(&h, Duration::from_secs(4)), Some(25.0));
        assert_eq!(cpu.update(&h, Duration::from_secs(4)), None);
    }
    #[test]
    fn aggregation_unknown_and_ballooning() {
        let h = host();
        let mut readings = BTreeMap::new();
        readings.insert(1, 1500);
        let data = snapshot(
            &h,
            400,
            &readings,
            Some(50.0),
            Some(Reading::Network(1234.5, 678.25)),
            1.0,
        )
        .unwrap();
        assert_eq!(data.cpu, Some(50.0));
        assert_eq!(data.total, 4000);
        assert_eq!(data.allocated, 3000);
        assert_eq!(data.used, 1400);
        assert_eq!(data.unknown, 1000);
        assert_eq!(data.overhead, 100);
        assert_eq!(data.period, 1.0);
        assert_eq!(data.network, Some((1234.5, 678.25)));
        let mut invalid = h;
        invalid.total = 10;
        assert!(snapshot(&invalid, 0, &readings, None, None, 1.0).is_err());
    }
    #[test]
    fn lifecycle_filtering_identity_and_names() {
        let mut h = host();
        h.domains[0].stubdom = Some(2);
        assert_eq!(eligible(&h).keys().copied().collect::<Vec<_>>(), vec![1]);
        h.domains[1].paused = true;
        assert!(eligible(&h).is_empty());
        h.domains[1].paused = false;
        h.domains[1].shutdown = true;
        assert!(eligible(&h).is_empty());
        h.domains[1].shutdown = false;
        h.domains[1].name = "bad;command".into();
        assert!(eligible(&h).is_empty());
    }
    #[test]
    fn atomic_publication_and_permissions() {
        let path = std::env::temp_dir().join(format!("mq-publish-{}", std::process::id()));
        publish(&path, "first").unwrap();
        publish(&path, "second").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn swap_coverage_ignores_missing_paused_and_reincarnated_readers() {
        let mut host = host();
        let mut pool = MemoryPool::new();
        pool.factory = |d| {
            Reader::spawn(
                CommandSpec {
                    source: "test swap".into(),
                    program: "python3".into(),
                    args: vec![
                        "-u".into(),
                        "-c".into(),
                        format!(
                "import time; print('M2 {} 1000 400 500 125',flush=True); time.sleep(30)", d.id),
                    ],
                },
                Kind::Memory(d.id),
                Timing::default(),
            )
        };
        pool.sync(&host);
        let deadline = Instant::now() + Duration::from_secs(3);
        while pool.readings(&host).len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let data = swap_snapshot(&host, (100, 50), Some(&pool));
        assert_eq!(data.expected, 3);
        assert_eq!(data.entries.len(), 3);
        assert_eq!(data.entries[0].name, "dom0");
        assert_eq!(data.entries[1].used, 375);
        assert_eq!(data.peak(), Some(75.0));
        assert_eq!(pool.readings(&host)[&1], 600); // Swap is never added to RAM.
        host.domains[1].identity = "new-identity".into();
        assert_eq!(swap_snapshot(&host, (0, 0), Some(&pool)).entries.len(), 2);
        host.domains[2].paused = true;
        let data = swap_snapshot(&host, (0, 0), Some(&pool));
        assert_eq!(data.expected, 2);
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.peak(), None);
        assert_eq!(swap_snapshot(&host, (0, 0), None).entries.len(), 1);
    }

    #[test]
    fn pool_reuses_retires_and_bounds_connections() {
        let mut pool = MemoryPool::new();
        pool.limit = 1;
        pool.factory = |d| {
            Reader::spawn(
                CommandSpec {
                    source: "test RAM".into(),
                    program: "python3".into(),
                    args: vec![
                        "-u".into(),
                        "-c".into(),
                        format!(
                            "import time; print('M1 {} 1000 400',flush=True); time.sleep(20)",
                            d.id
                        ),
                    ],
                },
                Kind::Memory(d.id),
                Timing::default(),
            )
        };
        let mut h = host();
        pool.sync(&h);
        assert_eq!(pool.readers.len(), 1);
        let deadline = Instant::now() + Duration::from_secs(3);
        while pool.readings(&h).get(&1) != Some(&600) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pool.readings(&h).get(&1), Some(&600));
        let key = pool.readers[&1].0.clone();
        pool.sync(&h);
        assert_eq!(pool.readers[&1].0, key);
        h.domains[1].identity = "new".into();
        assert!(pool.readings(&h).is_empty());
        h.domains[1].paused = true;
        pool.sync(&h);
        assert!(!pool.readers.contains_key(&1));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !pool.readers.contains_key(&2) && Instant::now() < deadline {
            pool.sync(&h);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(pool.readers.contains_key(&2));
        h.domains.clear();
        pool.sync(&h);
        assert!(pool.readers.is_empty());
    }
}
