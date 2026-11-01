//! Parses guest reports without Xen access. A collector tick is published once,
//! when its accepted requests finish or its 100 ms deadline expires.
use crate::metrics::handoff::{self, HostFrame, RawFrame};
use crate::metrics::protocol::{Kind, Reading, Tracker};
use crate::metrics::{eligible, publish, shutdown};
use multiload_qubes_snapshot::{unix_time_now, Snapshot};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use std::collections::{BTreeMap, BTreeSet};
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

struct PendingTick {
    frame: HostFrame,
    // None is a terminal reply with no valid measurement (including network warmup).
    replies: BTreeMap<String, Option<Reading>>,
}
impl PendingTick {
    fn load(input: &Path, now: f64) -> Result<Self, String> {
        let frame: HostFrame = serde_json::from_slice(&handoff::read_bounded(
            &input.join("host.json"),
            handoff::MAX_HOST_BYTES,
        )?)
        .map_err(|e| e.to_string())?;
        frame.sample.validate()?;
        multiload_qubes_snapshot::check_freshness(frame.sample.timestamp, frame.sample.period, now)
            .map_err(|e| e.to_string())?;
        if !frame.tick.deadline.is_finite() {
            return Err("invalid tick deadline".into());
        }
        Ok(Self {
            frame,
            replies: BTreeMap::new(),
        })
    }
    fn complete(&self) -> bool {
        self.replies.len() == self.frame.tick.expected.len()
    }
    fn snapshot(&self) -> Result<Snapshot, String> {
        let memory: BTreeMap<_, _> = eligible(&self.frame.sample.host)
            .keys()
            .filter_map(|id| {
                self.replies
                    .get(&format!("memory-{id}"))
                    .copied()
                    .flatten()
                    .map(|r| (*id, r))
            })
            .collect();
        let network = self.replies.get("network").copied().flatten();
        self.frame.sample.snapshot(&memory, network)
    }
}

impl Parser {
    fn parse(&mut self, file: &str, frame: RawFrame, kind: Kind) -> Option<Reading> {
        let report = self.reports.entry(file.into()).or_insert_with(|| Report {
            identity: frame.identity.clone(),
            session: frame.session.clone(),
            timestamp: 0.0,
            tracker: Tracker::new(kind),
            reading: None,
            last_error: None,
        });
        if report.identity != frame.identity || report.session != frame.session {
            *report = Report {
                identity: frame.identity,
                session: frame.session,
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
                    report.tracker = Tracker::new(kind);
                    None
                }
            };
        }
        report.reading
    }

    fn ingest(&mut self, input: &Path, pending: &mut PendingTick, file: &str) {
        let Some(identity) = pending.frame.tick.expected.get(file) else {
            return;
        };
        if pending.replies.contains_key(file) {
            return;
        }
        let Some(frame) =
            handoff::read_bounded(&input.join("raw").join(file), handoff::MAX_RAW_BYTES)
                .ok()
                .and_then(|b| serde_json::from_slice::<RawFrame>(&b).ok())
        else {
            return;
        };
        let Ok(now) = unix_time_now() else {
            return;
        };
        if frame.tick_id != pending.frame.tick.tick_id
            || frame.identity != *identity
            || frame.timestamp > pending.frame.tick.deadline
            || !handoff::fresh(
                frame.timestamp,
                now,
                multiload_qubes_snapshot::max_sample_age(pending.frame.sample.period),
            )
            || frame.payload.len() > handoff::MAX_REPORT_BYTES
        {
            return;
        }
        let kind = if file == "network" {
            Kind::Network
        } else {
            let Some(id) = file.strip_prefix("memory-").and_then(|s| s.parse().ok()) else {
                return;
            };
            Kind::Memory(id)
        };
        let reading = if frame.session == "disconnected" && frame.payload.is_empty() {
            self.reports.remove(file);
            None
        } else {
            self.parse(file, frame, kind)
        };
        pending.replies.insert(file.into(), reading);
    }
    fn rescan(&mut self, input: &Path, pending: &mut PendingTick) {
        let files: Vec<_> = pending.frame.tick.expected.keys().cloned().collect();
        for file in files {
            self.ingest(input, pending, &file);
        }
    }
    fn prune(&mut self, frame: &HostFrame) {
        let active = eligible(&frame.sample.host);
        self.reports.retain(|file, report| {
            if file == "network" {
                frame.network_identity.as_ref() == Some(&report.identity)
            } else {
                if !frame.memory_enabled {
                    return false;
                }
                let Some(id) = file
                    .strip_prefix("memory-")
                    .and_then(|s| s.parse::<u32>().ok())
                else {
                    return false;
                };
                active.get(&id).is_some_and(|key| {
                    file == &format!("memory-{id}")
                        && report.identity == format!("{id}/{}", key.uuid)
                })
            }
        });
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
    // Watch directories before the initial read; atomic publications replace inodes.
    let host_watch = watch
        .add_watch(&options.input, flags)
        .map_err(|e| e.to_string())?;
    let raw_watch = watch
        .add_watch(&options.input.join("raw"), flags)
        .map_err(|e| e.to_string())?;
    let mut parser = Parser::default();
    let mut pending: Option<PendingTick> = None;
    let mut finished_tick_id: Option<String> = None;
    let mut host_changed = true;
    let mut rescan = false;
    let mut changed: BTreeSet<String> = BTreeSet::new();
    let mut fresh_until = 0.0;
    let mut last_error = None;
    while !shutdown.requested() {
        let now = unix_time_now().map_err(|e| e.to_string())?;
        if host_changed {
            host_changed = false;
            match PendingTick::load(&options.input, now) {
                Ok(next) => {
                    last_error = None;
                    fresh_until = next.frame.sample.timestamp
                        + multiload_qubes_snapshot::max_sample_age(next.frame.sample.period);
                    let same_pending = pending
                        .as_ref()
                        .is_some_and(|p| p.frame.tick.tick_id == next.frame.tick.tick_id);
                    if !same_pending && finished_tick_id.as_ref() != Some(&next.frame.tick.tick_id)
                    {
                        // If this process was descheduled past a whole tick, finish
                        // the tick already in memory before moving to the new one.
                        if let Some(previous) = pending.take() {
                            publish_tick(&options.output, &previous)?;
                        }
                        parser.prune(&next.frame);
                        pending = Some(next);
                        rescan = true;
                    }
                }
                Err(error) => {
                    pending = None;
                    fresh_until = 0.0;
                    if last_error.as_ref() != Some(&error) {
                        eprintln!("Cannot parse guest metrics: {error}");
                        last_error = Some(error);
                    }
                }
            }
        }
        if let Some(tick) = pending.as_mut() {
            if rescan {
                parser.rescan(&options.input, tick);
            } else {
                for file in &changed {
                    parser.ingest(&options.input, tick, file);
                }
            }
            if tick.complete()
                || unix_time_now().map_err(|e| e.to_string())? >= tick.frame.tick.deadline
            {
                // One last bounded scan covers notifications not read yet. Only
                // reports stamped before the deadline can contribute.
                if !tick.complete() {
                    parser.rescan(&options.input, tick);
                }
                publish_tick(&options.output, tick)?;
                finished_tick_id = Some(tick.frame.tick.tick_id.clone());
                pending = None;
            }
        }
        rescan = false;
        changed.clear();
        if now > fresh_until {
            let _ = std::fs::remove_file(&options.output);
        }
        let timeout_ms = pending.as_ref().map_or(1000, |tick| {
            ((tick.frame.tick.deadline - unix_time_now().unwrap_or(now)).max(0.0) * 1000.0)
                .ceil()
                .min(1000.0) as u16
        });
        let mut fds = [PollFd::new(watch.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, timeout_ms) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(error.to_string()),
        }
        match watch.read_events() {
            Ok(events) => {
                for event in events {
                    if event.mask.intersects(
                        AddWatchFlags::IN_IGNORED
                            | AddWatchFlags::IN_DELETE_SELF
                            | AddWatchFlags::IN_MOVE_SELF,
                    ) {
                        return Err("input directory was replaced; restarting watches".into());
                    }
                    if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                        host_changed = true;
                        rescan = true;
                    }
                    if let Some(name) = event.name.and_then(|s| s.into_string().ok()) {
                        if event.wd == host_watch && name == "host.json" {
                            host_changed = true;
                        }
                        if event.wd == raw_watch {
                            changed.insert(name);
                        }
                    }
                }
            }
            Err(Errno::EAGAIN) | Err(Errno::EINTR) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn publish_tick(output: &Path, tick: &PendingTick) -> Result<(), String> {
    publish(
        output,
        &tick.snapshot()?.encode().map_err(|e| e.to_string())?,
    )
}
