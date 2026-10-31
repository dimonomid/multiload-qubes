//! Runs in dom0 and combines Xen counters with reports from running guests.
//!
//! Guest readers run on separate threads so a slow or stopped qube does not
//! block sampling. The panel reads a snapshot file; it never talks to guests.
mod diagnostics;
mod disk;
pub mod protocol;
mod shutdown;
pub mod stream;
pub mod xen;

use multiload_qubes_snapshot::{
    unix_time_now, DomainCpuSnapshot, DomainUsage, DomainUsageSnapshot, Snapshot,
};
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
    pub vcpus: Option<u32>,
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

/// Tracks counter differences, not CPU load averages.
/// Host usage uses the host CPU count; per-qube usage uses that qube's vCPUs.
#[derive(Default)]
pub struct CpuTracker {
    previous: BTreeMap<(u32, String), (u64, Option<u32>, bool)>,
    per_domain: Option<DomainCpuSnapshot>,
    time: Option<Duration>,
}
impl CpuTracker {
    pub fn update(&mut self, host: &Host, now: Duration) -> Option<f64> {
        let mut delta = 0.0;
        let mut current = BTreeMap::new();
        let active = eligible(host);
        let window_ns = self
            .time
            .and_then(|t| now.checked_sub(t))
            .and_then(|d| u64::try_from(d.as_nanos()).ok())
            .filter(|n| *n > 0 && *n <= 60_000_000_000);
        let mut usage = DomainUsageSnapshot {
            expected: active.len() as u32 + 1,
            entries: Vec::new(),
        };
        for domain in &host.domains {
            // Xen can reuse a domain ID for another qube. Do not subtract its
            // counters from the old qube's counters.
            let key = (domain.id, domain.identity.clone());
            let reporting = !domain.paused
                && !domain.shutdown
                && (domain.id == 0 || active.contains_key(&domain.id));
            if let Some((before, old_vcpus, was_reporting)) = self.previous.get(&key) {
                delta += domain.cpu_ns.saturating_sub(*before) as f64;
                // If the vCPU count changed, we do not know its capacity over
                // this whole interval. Start a fresh per-qube measurement.
                if reporting
                    && *was_reporting
                    && domain.vcpus == *old_vcpus
                    && usage.entries.len() < multiload_qubes_snapshot::MAX_DOMAIN_ENTRIES
                {
                    if let (Some(window), Some(vcpus), Some(used)) = (
                        window_ns,
                        domain.vcpus.filter(|n| *n > 0),
                        domain.cpu_ns.checked_sub(*before),
                    ) {
                        if let Some(total) = window.checked_mul(vcpus as u64) {
                            usage.entries.push(DomainUsage {
                                id: if domain.id == 0 {
                                    "dom0".into()
                                } else {
                                    format!("{}/{}", domain.id, domain.identity)
                                },
                                name: if domain.id == 0 {
                                    "dom0".into()
                                } else {
                                    domain.name.clone()
                                },
                                total,
                                used: used.min(total),
                            });
                        }
                    }
                }
            }
            current.insert(key, (domain.cpu_ns, domain.vcpus, reporting));
        }
        let elapsed = self
            .time
            .and_then(|t| now.checked_sub(t))
            .map(|t| t.as_secs_f64());
        self.per_domain = window_ns
            .filter(|_| host.cpus > 0)
            .map(|window_ns| DomainCpuSnapshot {
                window_ns,
                host_cpus: host.cpus,
                usage,
            });
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

/// Keeps reporters for running qubes and retires them when those qubes change.
/// The same report supplies guest RAM and swap; they need no separate connections.
pub struct MemoryPool {
    readers: BTreeMap<u32, (Identity, Reader)>,
    // Stopping a reader can take time. Keep it here so joining its thread
    // does not hold up the main sampling loop.
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
            // Count workers still shutting down too, so rapid VM restarts
            // cannot keep creating more threads and connections.
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
                    Some(Reading::Memory { used, .. }) => Some((d.id, used)),
                    _ => None,
                }
            })
            .collect()
    }
}
/// Missing reports stay absent; incarnation checks prevent old samples being reused.
fn domain_snapshot(
    host: &Host,
    dom0: (u64, u64),
    pool: Option<&MemoryPool>,
    value: fn(Reading) -> Option<(u64, u64)>,
) -> DomainUsageSnapshot {
    let active = eligible(host);
    let mut entries = vec![DomainUsage {
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
            if let Some((total, used)) = reader.value().and_then(value) {
                entries.push(DomainUsage {
                    id: format!("{}/{}", id, key.uuid),
                    name: key.name.clone(),
                    total,
                    used,
                });
            }
        }
    }
    DomainUsageSnapshot {
        expected: active.len() as u32 + 1,
        entries,
    }
}
fn swap_snapshot(host: &Host, dom0: (u64, u64), pool: Option<&MemoryPool>) -> DomainUsageSnapshot {
    domain_snapshot(host, dom0, pool, |r| match r {
        Reading::Memory { swap, .. } => swap,
        _ => None,
    })
}
fn ram_snapshot(host: &Host, dom0: (u64, u64), pool: Option<&MemoryPool>) -> DomainUsageSnapshot {
    domain_snapshot(host, dom0, pool, |r| match r {
        Reading::Memory { total, used, .. } => Some((total, used)),
        _ => None,
    })
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
        // The panel must see either the previous complete snapshot or this one.
        // Keeping the temporary file beside the destination allows an atomic rename.
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
    let mut disk = disk::Collector::default();
    let start = Instant::now();
    if options.once {
        let host = xen.sample()?;
        let meminfo = fs::read_to_string("/proc/meminfo").map_err(|e| e.to_string())?;
        let usage = protocol::dom0_memory(&meminfo)?;
        let mut data = snapshot(
            &host,
            usage.1,
            &BTreeMap::new(),
            None,
            None,
            options.interval,
        )?;
        data.swap = Some(swap_snapshot(&host, protocol::dom0_swap(&meminfo)?, None));
        data.qube_ram = Some(ram_snapshot(&host, usage, None));
        data.disk = disk.sample(start.elapsed());
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
            let cpu_usage = cpu.update(&host, start.elapsed());
            let readings = if let Some(pool) = &mut memory {
                pool.sync(&host);
                pool.readings(&host)
            } else {
                BTreeMap::new()
            };
            let meminfo = fs::read_to_string("/proc/meminfo").map_err(|e| e.to_string())?;
            let dom0 = protocol::dom0_memory(&meminfo)?;
            let mut data = snapshot(
                &host,
                dom0.1,
                &readings,
                cpu_usage,
                network.as_ref().and_then(Reader::value),
                options.interval,
            )?;
            data.swap = Some(swap_snapshot(
                &host,
                protocol::dom0_swap(&meminfo)?,
                memory.as_ref(),
            ));
            data.qube_cpu = cpu.per_domain.clone();
            data.qube_ram = Some(ram_snapshot(&host, dom0, memory.as_ref()));
            data.disk = disk.sample(start.elapsed());
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
            vcpus: Some(2),
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
    fn per_qube_cpu_normalizes_vcpus_and_handles_resize_pause_restart_and_reset() {
        let mut h = host();
        h.cpus = 16;
        h.domains[0].vcpus = Some(16);
        h.domains[1].vcpus = Some(4);
        h.domains[2].paused = true;
        let mut t = CpuTracker::default();
        assert_eq!(t.update(&h, Duration::ZERO), None);
        assert!(t.per_domain.is_none());
        h.domains[0].cpu_ns = 10_880_000_000;
        h.domains[1].cpu_ns = 2_800_000_000;
        assert!((t.update(&h, Duration::from_secs(1)).unwrap() - 85.5).abs() < 1e-9);
        let cpu = t.per_domain.as_ref().unwrap();
        assert_eq!(cpu.usage.expected, 2);
        let guest = cpu
            .usage
            .entries
            .iter()
            .find(|e| e.name == "qube-1")
            .unwrap();
        assert_eq!(cpu.vcpus(guest), 4);
        assert_eq!(guest.percent(), Some(70.0));
        assert!((cpu.host_percent(guest) - 17.5).abs() < 1e-9);
        h.domains[1].vcpus = Some(2);
        h.domains[1].cpu_ns += 1_400_000_000;
        t.update(&h, Duration::from_secs(2));
        assert_eq!(t.per_domain.as_ref().unwrap().usage.entries.len(), 1);
        h.domains[1].cpu_ns += 1_400_000_000;
        t.update(&h, Duration::from_secs(3));
        assert_eq!(
            t.per_domain.as_ref().unwrap().usage.entries[1].percent(),
            Some(70.0)
        );
        h.domains[1].paused = true;
        t.update(&h, Duration::from_secs(4));
        assert_eq!(t.per_domain.as_ref().unwrap().usage.expected, 1);
        h.domains[1].paused = false;
        t.update(&h, Duration::from_secs(5));
        assert_eq!(t.per_domain.as_ref().unwrap().usage.entries.len(), 1);
        h.domains[1].identity = "new-uuid".into();
        t.update(&h, Duration::from_secs(6));
        assert_eq!(t.per_domain.as_ref().unwrap().usage.entries.len(), 1);
        h.domains[1].cpu_ns += 2_000_000_000;
        t.update(&h, Duration::from_secs(7));
        let guest = &t.per_domain.as_ref().unwrap().usage.entries[1];
        assert_eq!(guest.percent(), Some(100.0));
        assert!(guest.id.contains("new-uuid"));
        h.domains[1].cpu_ns = 0;
        t.update(&h, Duration::from_secs(8));
        assert_eq!(t.per_domain.as_ref().unwrap().usage.entries.len(), 1);
        h.domains[1].vcpus = None;
        t.update(&h, Duration::from_secs(9));
        assert_eq!(t.per_domain.as_ref().unwrap().usage.expected, 2);
        assert_eq!(t.per_domain.as_ref().unwrap().usage.entries.len(), 1);
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
        let ram = ram_snapshot(&host, (2000, 400), Some(&pool));
        assert_eq!(ram.expected, 3);
        assert_eq!(ram.entries[0].total, 2000);
        assert_eq!(ram.entries[1].total, 1000);
        assert_eq!(ram.entries[1].used, 600);
        assert_eq!(ram.peak(), Some(60.0));
        host.domains[1].identity = "new-identity".into();
        assert_eq!(swap_snapshot(&host, (0, 0), Some(&pool)).entries.len(), 2);
        host.domains[2].paused = true;
        let data = swap_snapshot(&host, (0, 0), Some(&pool));
        assert_eq!(data.expected, 2);
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.peak(), None);
        let ram = ram_snapshot(&host, (2000, 400), Some(&pool));
        assert_eq!(ram.expected, 2);
        assert_eq!(ram.entries.len(), 1);
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
