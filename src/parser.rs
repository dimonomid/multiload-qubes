//! Parses guest bytes without opening Xen handles or launching guest commands.
//! Directory watches follow atomic renames; a full rescan also covers lost events.
use crate::metrics::handoff::{self, HostFrame, RawFrame};
use crate::metrics::protocol::{Kind, Reading, Tracker};
use crate::metrics::{eligible, publish, ram_snapshot, shutdown, snapshot, swap_snapshot};
use multiload_qubes_snapshot::{unix_time_now, Snapshot};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use std::collections::BTreeMap;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

struct Report {
    identity: String,
    session: String,
    timestamp: f64,
    tracker: Tracker,
    reading: Option<Reading>,
    last_error: Option<String>,
}
#[derive(Default)]
struct Parser {
    reports: BTreeMap<String, Report>,
}
impl Parser {
    fn read(
        &mut self,
        directory: &Path,
        file: &str,
        identity: &str,
        kind: Kind,
    ) -> Option<Reading> {
        let frame: RawFrame = handoff::read_bounded(&directory.join(file), handoff::MAX_RAW_BYTES)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())?;
        // A report may be published while we scan the directory. Check its
        // age after reading it, rather than against the start of the scan.
        let now = unix_time_now().ok()?;
        if frame.identity != identity
            || !handoff::fresh(frame.timestamp, now)
            || frame.payload.len() > handoff::MAX_REPORT_BYTES
        {
            self.reports.remove(file);
            return None;
        }
        let report = self.reports.entry(file.into()).or_insert_with(|| Report {
            identity: identity.into(),
            session: frame.session.clone(),
            timestamp: 0.0,
            tracker: Tracker::new(kind),
            reading: None,
            last_error: None,
        });
        if report.identity != identity || report.session != frame.session {
            *report = Report {
                identity: identity.into(),
                session: frame.session.clone(),
                timestamp: 0.0,
                tracker: Tracker::new(kind),
                reading: None,
                last_error: None,
            };
        }
        if report.timestamp != frame.timestamp {
            report.timestamp = frame.timestamp;
            report.reading = match report.tracker.update(&frame.payload) {
                Ok(reading) => {
                    if reading.is_some() && report.last_error.take().is_some() {
                        eprintln!("Guest metrics [{file}] recovered");
                    }
                    reading
                }
                Err(error) => {
                    if report.last_error.as_ref() != Some(&error) {
                        eprintln!("Guest metrics [{file}] invalid: {error}");
                        report.last_error = Some(error);
                    }
                    // Do not retain a network baseline across malformed reports.
                    report.tracker = Tracker::new(kind);
                    None
                }
            };
        }
        report.reading
    }

    fn collect(&mut self, input: &Path, now: f64) -> Result<Snapshot, String> {
        let frame: HostFrame = serde_json::from_slice(&handoff::read_bounded(
            &input.join("host.json"),
            handoff::MAX_HOST_BYTES,
        )?)
        .map_err(|e| e.to_string())?;
        let base = Snapshot::decode_fresh_at(&frame.metrics, now).map_err(|e| e.to_string())?;
        let raw = input.join("raw");
        let mut memory = BTreeMap::new();
        let mut wanted = std::collections::BTreeSet::new();
        let active = eligible(&frame.host);
        for (id, key) in active
            .iter()
            .take(if frame.memory_enabled { 128 } else { 0 })
        {
            let file = format!("memory-{id}");
            wanted.insert(file.clone());
            if let Some(reading) = self.read(
                &raw,
                &file,
                &format!("{id}/{}", key.uuid),
                Kind::Memory(*id),
            ) {
                memory.insert(*id, reading);
            }
        }
        // The network report carries a root-supplied incarnation key. Match it
        // to a currently running guest before using its counters.
        let network = frame
            .network_identity
            .as_deref()
            .and_then(|identity| self.read(&raw, "network", identity, Kind::Network));
        wanted.insert("network".into());
        self.reports.retain(|file, _| wanted.contains(file));
        let usage = memory
            .iter()
            .filter_map(|(id, r)| match r {
                Reading::Memory { used, .. } => Some((*id, *used)),
                _ => None,
            })
            .collect();
        let mut combined = snapshot(
            &frame.host,
            frame.dom0_ram.1,
            &usage,
            base.cpu,
            network,
            base.period,
        )?;
        // Keep the host's timestamp: a live parser must not make stale host data
        // appear fresh when the privileged collector has stopped reporting.
        combined.timestamp = base.timestamp;
        combined.qube_cpu = base.qube_cpu;
        combined.disk = base.disk;
        combined.qube_ram = Some(ram_snapshot(&frame.host, frame.dom0_ram, &memory));
        combined.swap = Some(swap_snapshot(&frame.host, frame.dom0_swap, &memory));
        Ok(combined)
    }
}

pub struct Options {
    pub input: PathBuf,
    pub output: PathBuf,
}

pub fn run(options: Options) -> Result<(), String> {
    let shutdown = shutdown::Shutdown::register().map_err(|e| e.to_string())?;
    let watch =
        Inotify::init(InitFlags::IN_CLOEXEC | InitFlags::IN_NONBLOCK).map_err(|e| e.to_string())?;
    let flags = AddWatchFlags::IN_MOVED_TO
        | AddWatchFlags::IN_DELETE
        | AddWatchFlags::IN_DELETE_SELF
        | AddWatchFlags::IN_MOVE_SELF;
    // Install both watches before reading anything, including on parser restart.
    watch
        .add_watch(&options.input, flags)
        .map_err(|e| e.to_string())?;
    watch
        .add_watch(&options.input.join("raw"), flags)
        .map_err(|e| e.to_string())?;
    let mut parser = Parser::default();
    let mut last_error = None;
    while !shutdown.requested() {
        let result = unix_time_now()
            .map_err(|e| e.to_string())
            .and_then(|now| parser.collect(&options.input, now))
            .and_then(|snapshot| snapshot.encode().map_err(|e| e.to_string()))
            .and_then(|text| publish(&options.output, &text));
        match result {
            Ok(()) => {
                last_error = None;
            }
            Err(error) => {
                // Never leave the last combined snapshot looking usable after
                // its host input goes missing or becomes invalid.
                let _ = std::fs::remove_file(&options.output);
                if last_error.as_ref() != Some(&error) {
                    eprintln!("Cannot parse guest metrics: {error}");
                    last_error = Some(error);
                }
            }
        }
        let mut fds = [PollFd::new(watch.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, 1000_u16) {
            Ok(0) => continue, // Expire guest reports even if all writers stop.
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(error.to_string()),
        }
        match watch.read_events() {
            Ok(events) => {
                if events.iter().any(|e| {
                    e.mask.intersects(
                        AddWatchFlags::IN_IGNORED
                            | AddWatchFlags::IN_DELETE_SELF
                            | AddWatchFlags::IN_MOVE_SELF,
                    )
                }) {
                    return Err("input directory was replaced; restarting watches".into());
                }
                // Always rescan, including IN_Q_OVERFLOW. Events are hints,
                // not a queue of reports that must each be replayed.
            }
            Err(Errno::EAGAIN) | Err(Errno::EINTR) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}
