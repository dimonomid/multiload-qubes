//! Privileged host sampling and guest transport.
//!
//! Transport workers publish bounded raw reports without interpreting measurements.
//! The systemd parser service combines them under a dedicated, restricted account.
mod diagnostics;
mod disk;
pub mod stream;
pub mod xen;

use crate::metrics::{
    eligible, handoff, protocol, publish, ram_snapshot, shutdown, snapshot, swap_snapshot, Domain,
    Host, Identity, MAX_NUM_QUBE_REPORTERS, NUM_NETWORK_REPORTERS,
};
use multiload_qubes_snapshot::{DomainCpuSnapshot, DomainUsage, DomainUsageSnapshot};
use protocol::Kind;
#[cfg(test)]
use protocol::Reading;
use std::collections::BTreeMap;
use std::fs;
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};
use stream::{CommandSpec, Reader, Timing};

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

// Retired workers count too, so reconnects cannot exceed the transport budget.
const MAX_NUM_REPORTER_WORKERS: usize = MAX_NUM_QUBE_REPORTERS + NUM_NETWORK_REPORTERS;

/// Owns only transports; measurement fields are parsed by the other service.
struct Reporters {
    readers: BTreeMap<String, (Identity, Reader)>,
    retired: Vec<Reader>,
    directory: std::path::PathBuf,
    factory: fn(&Identity, Kind) -> CommandSpec,
    limit: usize,
}
impl Reporters {
    fn new(directory: std::path::PathBuf) -> Self {
        Self {
            readers: BTreeMap::new(),
            retired: Vec::new(),
            directory,
            factory: |identity, kind| CommandSpec::qube(&identity.name, kind),
            limit: MAX_NUM_REPORTER_WORKERS,
        }
    }
    fn sync(&mut self, host: &Host, memory: bool, network: Option<&str>) {
        let mut wanted = BTreeMap::new();
        let mut memory_count = 0;
        for identity in eligible(host).into_values() {
            if memory && memory_count < MAX_NUM_QUBE_REPORTERS {
                memory_count += 1;
                wanted.insert(
                    format!("memory-{}", identity.id),
                    (identity.clone(), Kind::Memory(identity.id)),
                );
            }
            if network == Some(identity.name.as_str()) {
                wanted.insert("network".into(), (identity, Kind::Network));
            }
        }
        let remove: Vec<_> = self
            .readers
            .iter()
            .filter(|(file, (key, _))| wanted.get(*file).is_none_or(|(new, _)| new != key))
            .map(|(file, _)| file.clone())
            .collect();
        for file in remove {
            if let Some((_, reader)) = self.readers.remove(&file) {
                reader.stop();
                self.retired.push(reader);
            }
        }
        self.retired.retain(|r| !r.finished());
        // Remove abandoned files so domain-ID churn cannot grow /run forever.
        if let Ok(files) = fs::read_dir(&self.directory) {
            for file in files.flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                let report_file = name == "network"
                    || name
                        .strip_prefix("memory-")
                        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()));
                // A worker may be publishing a temporary file right now.
                if report_file && !wanted.contains_key(&name) {
                    let _ = fs::remove_file(file.path());
                }
            }
        }
        for (file, (identity, kind)) in wanted {
            if self.readers.contains_key(&file) {
                continue;
            }
            // Count workers still shutting down so rapid restarts stay bounded.
            if self.readers.len() + self.retired.len() >= self.limit {
                break;
            }
            let target = stream::RawTarget {
                path: self.directory.join(&file),
                identity: format!("{}/{}", identity.id, identity.uuid),
            };
            match Reader::spawn((self.factory)(&identity, kind), Timing::default(), target) {
                Ok(reader) => {
                    self.readers.insert(file, (identity, reader));
                }
                Err(error) => eprintln!("Cannot start guest reporter: {error}"),
            }
        }
    }
    fn request(&self, tick_id: &str) -> BTreeMap<String, String> {
        let mut expected = BTreeMap::new();
        for (file, (identity, reader)) in &self.readers {
            if reader.request(tick_id) {
                expected.insert(file.clone(), format!("{}/{}", identity.id, identity.uuid));
            }
        }
        expected
    }
}
impl Drop for Reporters {
    fn drop(&mut self) {
        for (_, reader) in self.readers.values() {
            reader.stop();
        }
        for reader in &self.retired {
            reader.stop();
        }
    }
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
        data.swap = Some(swap_snapshot(
            &host,
            protocol::dom0_swap(&meminfo)?,
            &BTreeMap::new(),
        ));
        data.qube_ram = Some(ram_snapshot(&host, usage, &BTreeMap::new()));
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
    let raw_directory = parent.join("raw");
    fs::create_dir_all(&raw_directory).map_err(|e| e.to_string())?;
    let mut reporters = Reporters::new(raw_directory);
    let instance = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos()
    );
    let mut sequence = 0_u64;
    while !shutdown.requested() {
        let tick_started = Instant::now();
        let deadline = multiload_qubes_snapshot::unix_time_now().map_err(|e| e.to_string())?
            + handoff::TICK_WAIT_SECONDS;
        sequence += 1;
        let tick_id = format!("{instance}/{sequence}");
        let result = (|| {
            let host = xen.sample()?;
            let cpu_usage = cpu.update(&host, start.elapsed());
            reporters.sync(&host, options.memory, options.network.as_deref());
            let expected = reporters.request(&tick_id);
            let readings = BTreeMap::new();
            let meminfo = fs::read_to_string("/proc/meminfo").map_err(|e| e.to_string())?;
            let dom0 = protocol::dom0_memory(&meminfo)?;
            let mut data = snapshot(&host, dom0.1, &readings, cpu_usage, None, options.interval)?;
            data.swap = Some(swap_snapshot(
                &host,
                protocol::dom0_swap(&meminfo)?,
                &BTreeMap::new(),
            ));
            data.qube_cpu = cpu.per_domain.clone();
            data.qube_ram = Some(ram_snapshot(&host, dom0, &BTreeMap::new()));
            data.disk = disk.sample(start.elapsed());
            let metrics = data.encode().map_err(|e| e.to_string())?;
            let frame = handoff::HostFrame {
                tick: handoff::Tick {
                    tick_id,
                    deadline,
                    expected,
                },
                host: host.clone(),
                metrics,
                memory_enabled: options.memory,
                network_identity: eligible(&host)
                    .values()
                    .find(|key| options.network.as_deref() == Some(key.name.as_str()))
                    .map(|key| format!("{}/{}", key.id, key.uuid)),
                dom0_ram: dom0,
                dom0_swap: protocol::dom0_swap(&meminfo)?,
            };
            // Publish context first. Raw reports from a previous incarnation are
            // rejected by identity even if an old file remains in the directory.
            publish(
                &parent.join("host.json"),
                &serde_json::to_string(&frame).map_err(|e| e.to_string())?,
            )?;
            publish(&options.output, &frame.metrics)
        })();
        if let Err(error) = result {
            eprintln!("Cannot collect metrics: {error}");
        }
        while !shutdown.requested() && tick_started.elapsed().as_secs_f64() < options.interval {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    drop(reporters);
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
    fn guest_aggregation_keeps_dom0_and_missing_coverage() {
        let host = host();
        let readings = BTreeMap::from([(
            1,
            Reading::Memory {
                total: 1000,
                used: 600,
                swap: Some((500, 375)),
            },
        )]);
        let swap = swap_snapshot(&host, (100, 50), &readings);
        assert_eq!(swap.expected, 3);
        assert_eq!(swap.entries.len(), 2);
        assert_eq!(swap.peak(), Some(75.0));
        let ram = ram_snapshot(&host, (2000, 400), &readings);
        assert_eq!(ram.entries[1].used, 600);
        assert_eq!(ram.peak(), Some(60.0));
    }
    #[test]
    fn raw_reporters_pass_unparsed_bytes_reuse_connections_and_bound_retired_workers() {
        let directory = std::env::temp_dir().join(format!("mq-raw-pool-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let mut pool = Reporters::new(directory.clone());
        pool.limit = 1;
        pool.factory = |_, _| {
            CommandSpec { source: "raw transport test".into(), program: "python3".into(), args: vec!["-u".into(),"-c".into(),"import sys,time; sys.stdin.readline(); sys.stdout.buffer.write(b'not a metric: \\xff\\n'); sys.stdout.flush(); time.sleep(20)".into()] }
        };
        let mut h = host();
        pool.sync(&h, true, None);
        pool.request("test-tick");
        assert_eq!(pool.readers.len(), 1);
        let deadline = Instant::now() + Duration::from_secs(3);
        let frame = loop {
            pool.request("test-tick");
            if let Ok(bytes) = fs::read(directory.join("memory-1")) {
                break serde_json::from_slice::<handoff::RawFrame>(&bytes).unwrap();
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(frame.payload, b"not a metric: \xff");
        assert_eq!(frame.identity, "1/uuid-1");
        pool.sync(&h, true, None);
        pool.request("test-tick");
        let same: handoff::RawFrame =
            serde_json::from_slice(&fs::read(directory.join("memory-1")).unwrap()).unwrap();
        assert_eq!(same.session, frame.session);
        fs::write(directory.join(".metrics-in-progress"), b"temporary").unwrap();
        h.domains[1].identity = "replacement".into();
        pool.sync(&h, true, None);
        pool.request("test-tick");
        assert!(directory.join(".metrics-in-progress").exists());
        let deadline = Instant::now() + Duration::from_secs(3);
        while pool
            .readers
            .get("memory-1")
            .is_none_or(|(key, _)| key.uuid != "replacement")
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
            pool.sync(&h, true, None);
            pool.request("test-tick");
        }
        h.domains[1].paused = true;
        pool.sync(&h, true, None);
        pool.request("test-tick");
        assert!(pool.readers.len() + pool.retired.len() <= 1);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !pool.readers.contains_key("memory-2") {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
            pool.sync(&h, true, None);
            pool.request("test-tick");
        }
        h.domains.clear();
        pool.sync(&h, true, None);
        pool.request("test-tick");
        assert!(pool.readers.is_empty());
        drop(pool);
        fs::remove_dir_all(directory).unwrap();
    }
}
