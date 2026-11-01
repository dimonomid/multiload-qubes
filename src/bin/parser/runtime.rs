//! Parses guest reports without Xen access. A collector tick is published once,
//! when its accepted requests finish or its 100 ms deadline expires.
use crate::metrics::handoff::{self, HostFrame, RawFrame};
use crate::metrics::protocol::{Reading, Tracker};
use crate::metrics::{eligible, publish, shutdown, QubeIdentity, ReporterKey};
use multiload_qubes_snapshot::{unix_time_now, Snapshot};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

/// Parsing state retained across ticks for one reporter connection.
struct Report {
    /// Qube whose counters this state belongs to.
    identity: QubeIdentity,
    /// Connection key; changing it discards the old network baseline.
    session: String,
    /// Receipt time of the last parsed frame, used to avoid parsing it twice.
    timestamp: f64,
    /// Protocol checks and previous network counters.
    tracker: Tracker,
    /// Result for timestamp; None also covers valid network warmup.
    reading: Option<Reading>,
    /// Last logged parse error, so repeated failures do not flood the journal.
    last_error: Option<String>,
}
/// Per-reporter parsing state reused across ticks; pending replies live in PendingTick.
#[derive(Default)]
struct Parser {
    /// Trackers for current reporters, pruned when host identities or options change.
    reports: BTreeMap<ReporterKey, Report>,
}

/// Host data and terminal replies for the one tick being assembled.
struct PendingTick {
    /// Root-owned host sample and its shared reply deadline.
    frame: HostFrame,
    // None is a terminal reply with no valid measurement (including network warmup).
    replies: BTreeMap<ReporterKey, Option<Reading>>,
}
impl PendingTick {
    /// Read and validate host.json before accepting its tick or measurements.
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
    /// Invalid replies and network warmup count as finished too.
    fn complete(&self) -> bool {
        self.replies.len() == self.frame.tick.expected.len()
    }
    /// Build output from this tick only; do not reuse older RAM or rate readings.
    fn snapshot(&self) -> Result<Snapshot, String> {
        let memory: BTreeMap<_, _> = eligible(&self.frame.sample.host)
            .keys()
            .filter_map(|id| {
                self.replies
                    .get(&ReporterKey::QubeMetrics(*id))
                    .copied()
                    .flatten()
                    .map(|r| (*id, r))
            })
            .collect();
        let network = self.replies.get(&ReporterKey::Network).copied().flatten();
        self.frame.sample.snapshot(&memory, network)
    }
}

impl Parser {
    /// Reset state on identity/session changes and log each distinct error once.
    fn parse(&mut self, key: ReporterKey, frame: RawFrame) -> Option<Reading> {
        let report = self.reports.entry(key).or_insert_with(|| Report {
            identity: frame.identity.clone(),
            session: frame.session.clone(),
            timestamp: 0.0,
            tracker: Tracker::new(key),
            reading: None,
            last_error: None,
        });
        if report.identity != frame.identity || report.session != frame.session {
            *report = Report {
                identity: frame.identity,
                session: frame.session,
                timestamp: 0.0,
                tracker: Tracker::new(key),
                reading: None,
                last_error: None,
            };
        }
        if report.timestamp != frame.timestamp {
            report.timestamp = frame.timestamp;
            report.reading = match report.tracker.update(&frame.payload) {
                Ok(reading) => {
                    if reading.is_some() && report.last_error.take().is_some() {
                        eprintln!("Guest metrics [{key}] recovered");
                    }
                    reading
                }
                Err(error) => {
                    if report.last_error.as_ref() != Some(&error) {
                        eprintln!("Guest metrics [{key}] invalid: {error}");
                        report.last_error = Some(error);
                    }
                    report.tracker = Tracker::new(key);
                    None
                }
            };
        }
        report.reading
    }

    /// Accept one matching, on-time reply; unrelated or late files leave the tick waiting.
    fn ingest(&mut self, input: &Path, pending: &mut PendingTick, key: ReporterKey) {
        // A recognized reporter may still be busy with an older tick. Only
        // connections accepted for this tick belong in its result.
        let Some(identity) = pending.frame.tick.expected.get(&key) else {
            return;
        };
        if pending.replies.contains_key(&key) {
            return;
        }
        let Some(frame) = handoff::read_bounded(
            &input.join("raw").join(key.to_string()),
            handoff::MAX_RAW_BYTES,
        )
        .ok()
        .and_then(|b| serde_json::from_slice::<RawFrame>(&b).ok()) else {
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
        let reading = if frame.session == "disconnected" && frame.payload.is_empty() {
            self.reports.remove(&key);
            None
        } else {
            self.parse(key, frame)
        };
        pending.replies.insert(key, reading);
    }
    /// Check expected files after loading host.json or before closing a timed-out tick.
    fn rescan(&mut self, input: &Path, pending: &mut PendingTick) {
        let keys: Vec<_> = pending.frame.tick.expected.keys().copied().collect();
        for key in keys {
            self.ingest(input, pending, key);
        }
    }
    /// Drop baselines belonging to stopped qubes, replaced connections, or disabled reporters.
    fn prune(&mut self, frame: &HostFrame) {
        let active = eligible(&frame.sample.host);
        self.reports.retain(|key, report| match key {
            ReporterKey::Network => frame.network_identity.as_ref() == Some(&report.identity),
            ReporterKey::QubeMetrics(id) => {
                frame.memory_enabled
                    && active
                        .get(id)
                        .is_some_and(|qube| report.identity == qube.identity)
            }
        });
    }
}

/// Paths supplied by the parser service command line.
pub struct Options {
    /// Root-owned directory containing host.json and raw/.
    pub input: PathBuf,
    /// Combined snapshot read by the panel.
    pub output: PathBuf,
}

/// Wait for file changes and tick deadlines, publishing at most once per tick.
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
    let mut changed: BTreeSet<ReporterKey> = BTreeSet::new();
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
                for key in &changed {
                    parser.ingest(&options.input, tick, *key);
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
        // Use integer milliseconds rather than Duration here because poll takes that unit.
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
                            // Failed writes can delete temporary files too.
                            // Those events do not name a published report.
                            if let Ok(key) = name.parse::<ReporterKey>() {
                                changed.insert(key);
                            }
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

/// Preserve the host timestamp when publishing the combined snapshot.
fn publish_tick(output: &Path, tick: &PendingTick) -> Result<(), String> {
    publish(
        output,
        &tick.snapshot()?.encode().map_err(|e| e.to_string())?,
    )
}
