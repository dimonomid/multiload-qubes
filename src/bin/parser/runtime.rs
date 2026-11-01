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
use std::time::Duration;

/// Last valid measurement and its original receipt time. Reusing it does not make it newer.
#[derive(Clone, Copy)]
struct CachedReading {
    reading: Reading,
    received_ns: u64,
}
/// Use the original monotonic receipt time; wall-clock changes must not extend cache life.
fn report_fresh(received_ns: u64, now_ns: u64, period: f64) -> bool {
    now_ns.checked_sub(received_ns).is_some_and(|age| {
        Duration::from_nanos(age)
            <= Duration::from_secs_f64(multiload_qubes_snapshot::max_sample_age(period))
    })
}

/// Parsing state retained across ticks for one reporter connection.
struct Report {
    /// Qube whose counters this state belongs to.
    identity: QubeIdentity,
    /// Connection key; changing it discards old readings and network counters.
    session: String,
    /// Last processed frame, including invalid reports, to avoid parsing it twice.
    received_ns: Option<u64>,
    /// Protocol checks and the previous network counters.
    tracker: Tracker,
    /// Last valid reading; invalid reports must not refresh its age.
    sample: Option<CachedReading>,
    /// Last logged parse error, so repeated failures do not flood the journal.
    last_error: Option<String>,
}
#[derive(Default)]
struct Parser {
    /// Connection state and cached readings, pruned when host identities or options change.
    reports: BTreeMap<ReporterKey, Report>,
}

/// Latest host context, retained after publication so late replies can still be checked.
struct CurrentTick {
    frame: HostFrame,
    /// Allowed reporters and their current qube identities, including busy connections.
    sources: BTreeMap<ReporterKey, QubeIdentity>,
    /// Only replies for this tick count toward finishing its wait early.
    replies: BTreeSet<ReporterKey>,
    /// Late reports update the cache but must not publish this tick a second time.
    published: bool,
}
impl CurrentTick {
    fn load(input: &Path, now: f64) -> Result<Self, String> {
        let frame: HostFrame = serde_json::from_slice(&handoff::read_bounded(
            &input.join("host.json"),
            handoff::MAX_HOST_BYTES,
        )?)
        .map_err(|e| e.to_string())?;
        frame.sample.validate()?;
        multiload_qubes_snapshot::check_freshness(frame.sample.timestamp, frame.sample.period, now)
            .map_err(|e| e.to_string())?;
        let mut sources = BTreeMap::new();
        if frame.memory_enabled {
            for (id, qube) in eligible(&frame.sample.host)
                .into_iter()
                .take(crate::metrics::MAX_NUM_QUBE_REPORTERS)
            {
                sources.insert(ReporterKey::QubeMetrics(id), qube.identity);
            }
        }
        if let Some(identity) = &frame.network_identity {
            sources.insert(ReporterKey::Network, identity.clone());
        }
        Ok(Self {
            frame,
            sources,
            replies: BTreeSet::new(),
            published: false,
        })
    }
    /// Invalid measurements and network warmup count as replies too.
    fn complete(&self) -> bool {
        self.replies.len() == self.frame.tick.expected.len()
    }
    fn snapshot(&self, parser: &Parser, now_ns: u64) -> Result<Snapshot, String> {
        let reading = |key| {
            let report = parser.reports.get(&key)?;
            if self.sources.get(&key) != Some(&report.identity) {
                return None;
            }
            let sample = report.sample?;
            report_fresh(sample.received_ns, now_ns, self.frame.sample.period)
                .then_some(sample.reading)
        };
        let memory = self
            .sources
            .keys()
            .filter_map(|key| match key {
                ReporterKey::QubeMetrics(id) => reading(*key).map(|r| (*id, r)),
                ReporterKey::Network => None,
            })
            .collect();
        self.frame
            .sample
            .snapshot(&memory, reading(ReporterKey::Network))
    }
}

impl Parser {
    /// Reset on connection changes. Invalid replies do not extend the cached reading's life.
    fn parse(&mut self, key: ReporterKey, frame: RawFrame) {
        // A delayed file from an older session must not reset a newer connection.
        if self
            .reports
            .get(&key)
            .and_then(|r| r.received_ns)
            .is_some_and(|stamp| stamp >= frame.received_ns)
        {
            return;
        }
        if frame.session == "disconnected" && frame.payload.is_empty() {
            self.reports.remove(&key);
            return;
        }
        let report = self.reports.entry(key).or_insert_with(|| Report {
            identity: frame.identity.clone(),
            session: frame.session.clone(),
            received_ns: None,
            tracker: Tracker::new(key),
            sample: None,
            last_error: None,
        });
        if report.identity != frame.identity || report.session != frame.session {
            *report = Report {
                identity: frame.identity,
                session: frame.session,
                received_ns: None,
                tracker: Tracker::new(key),
                sample: None,
                last_error: None,
            };
        }
        report.received_ns = Some(frame.received_ns);
        match report.tracker.update(&frame.payload) {
            Ok(reading) => {
                if reading.is_some() && report.last_error.take().is_some() {
                    eprintln!("Guest metrics [{key}] recovered");
                }
                // A valid network reset/warmup clears the old rate until a new delta exists.
                report.sample = reading.map(|reading| CachedReading {
                    reading,
                    received_ns: frame.received_ns,
                });
            }
            Err(error) => {
                if report.last_error.as_ref() != Some(&error) {
                    eprintln!("Guest metrics [{key}] invalid: {error}");
                    report.last_error = Some(error);
                }
                report.tracker = Tracker::new(key);
            }
        }
    }

    /// Cache valid reports even after their tick has finished. Only matching on-time
    /// replies shorten the current tick's wait; cached data does not count as a new reply.
    fn ingest(&mut self, input: &Path, tick: &mut CurrentTick, key: ReporterKey) {
        let Some(identity) = tick.sources.get(&key) else {
            return;
        };
        let Some(frame) = handoff::read_bounded(
            &input.join("raw").join(key.to_string()),
            handoff::MAX_RAW_BYTES,
        )
        .ok()
        .and_then(|b| serde_json::from_slice::<RawFrame>(&b).ok()) else {
            return;
        };
        let Ok(now_ns) = handoff::monotonic_ns() else {
            return;
        };
        if frame.identity != *identity
            || !report_fresh(frame.received_ns, now_ns, tick.frame.sample.period)
            || frame.payload.len() > handoff::MAX_REPORT_BYTES
        {
            return;
        }
        if frame.tick_id == tick.frame.tick.tick_id
            && tick.frame.tick.expected.get(&key) == Some(identity)
            && frame.received_ns <= tick.frame.tick.deadline_ns
        {
            tick.replies.insert(key);
        }
        self.parse(key, frame);
    }
    /// Include busy reporters' files, not just the requests accepted for this tick.
    fn rescan(&mut self, input: &Path, tick: &mut CurrentTick) {
        let keys: Vec<_> = tick.sources.keys().copied().collect();
        for key in keys {
            self.ingest(input, tick, key);
        }
    }
    fn prune(&mut self, tick: &CurrentTick) {
        self.reports
            .retain(|key, report| tick.sources.get(key) == Some(&report.identity));
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
    let mut current: Option<CurrentTick> = None;
    let mut host_changed = true;
    let mut rescan = false;
    let mut changed: BTreeSet<ReporterKey> = BTreeSet::new();
    let mut fresh_until = 0.0;
    let mut last_error = None;
    while !shutdown.requested() {
        let now = unix_time_now().map_err(|e| e.to_string())?;
        if host_changed {
            host_changed = false;
            match CurrentTick::load(&options.input, now) {
                Ok(next) => {
                    last_error = None;
                    fresh_until = next.frame.sample.timestamp
                        + multiload_qubes_snapshot::max_sample_age(next.frame.sample.period);
                    let same_tick = current
                        .as_ref()
                        .is_some_and(|t| t.frame.tick.tick_id == next.frame.tick.tick_id);
                    if !same_tick {
                        // If this process was descheduled past a whole tick, finish
                        // the tick already in memory before moving to the new one.
                        if let Some(previous) = current.take().filter(|t| !t.published) {
                            publish_tick(&options.output, &previous, &parser)?;
                        }
                        parser.prune(&next);
                        current = Some(next);
                        rescan = true;
                    }
                }
                Err(error) => {
                    current = None;
                    parser.reports.clear();
                    fresh_until = 0.0;
                    if last_error.as_ref() != Some(&error) {
                        eprintln!("Cannot parse guest metrics: {error}");
                        last_error = Some(error);
                    }
                }
            }
        }
        if let Some(tick) = current.as_mut() {
            if rescan {
                parser.rescan(&options.input, tick);
            } else {
                for key in &changed {
                    parser.ingest(&options.input, tick, *key);
                }
            }
            if !tick.published
                && (tick.complete() || handoff::monotonic_ns()? >= tick.frame.tick.deadline_ns)
            {
                // Pick up any report notifications not read yet before taking the snapshot.
                // The cutoff limits waiting, not the usefulness of a recent cached report.
                if !tick.complete() {
                    parser.rescan(&options.input, tick);
                }
                publish_tick(&options.output, tick, &parser)?;
                tick.published = true;
            }
        }
        rescan = false;
        changed.clear();
        if now > fresh_until {
            let _ = std::fs::remove_file(&options.output);
        }
        // poll takes milliseconds; round up so a partial millisecond does not spin.
        let now_ns = handoff::monotonic_ns()?;
        let timeout_ms = current
            .as_ref()
            .filter(|t| !t.published)
            .map_or(1000, |tick| {
                tick.frame
                    .tick
                    .deadline_ns
                    .saturating_sub(now_ns)
                    .div_ceil(1_000_000)
                    .min(1000) as u16
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
fn publish_tick(output: &Path, tick: &CurrentTick, parser: &Parser) -> Result<(), String> {
    publish(
        output,
        &tick
            .snapshot(parser, handoff::monotonic_ns()?)?
            .encode()
            .map_err(|e| e.to_string())?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity() -> QubeIdentity {
        QubeIdentity {
            domain_id: 1,
            vm_path: "/vm/test".into(),
        }
    }
    fn raw(session: &str, received_ns: u64, payload: &[u8]) -> RawFrame {
        RawFrame {
            tick_id: "old-tick".into(),
            identity: identity(),
            session: session.into(),
            received_ns,
            timestamp: 1000.0,
            payload: payload.into(),
        }
    }
    fn tick() -> CurrentTick {
        let domain = |id| {
            json!({"id":id,"identity":"/vm/test","name":format!("qube-{id}"),
            "cpu_ns":0,"vcpus":2,"allocated":1000,"paused":false,"shutdown":false,"stubdom":null})
        };
        let frame = serde_json::from_value(json!({
            "tick":{"tick_id":"new-tick","deadline_ns":2_000_000_000_u64,"expected":{}},
            "sample":{"timestamp":1000.0,"period":1.0,
                "host":{"total":4000,"free":2000,"cpus":4,"domains":[domain(0),domain(1)]},
                "cpu":25.0,"qube_cpu":null,"disk":null,"dom0_ram":[1000,100],"dom0_swap":[100,50]},
            "memory_enabled":true,"network_identity":identity(),
        }))
        .unwrap();
        CurrentTick {
            frame,
            sources: BTreeMap::from([
                (ReporterKey::QubeMetrics(1), identity()),
                (ReporterKey::Network, identity()),
            ]),
            replies: BTreeSet::new(),
            published: false,
        }
    }
    const MEMORY: &[u8] = b"M2 1 1000 400 500 125";
    const NETWORK: &[u8] = b"N1 1000000000 12345678-1234-1234-1234-123456789abc 1:100:200";

    #[test]
    fn reuse_duplicates_and_invalid_reports_do_not_refresh_cache_age() {
        let mut parser = Parser::default();
        let tick = tick();
        let key = ReporterKey::QubeMetrics(1);
        parser.parse(key, raw("a", 1_000_000_000, MEMORY));
        for now_ns in [2_000_000_000, 5_000_000_000, 6_000_000_000] {
            assert_eq!(tick.snapshot(&parser, now_ns).unwrap().used, 700);
        }
        parser.parse(key, raw("a", 1_000_000_000, MEMORY));
        parser.parse(key, raw("a", 6_000_000_000, b"invalid"));
        assert_eq!(tick.snapshot(&parser, 6_000_000_001).unwrap().unknown, 1000);
        // The same original report can still be within the limit for a longer interval.
        let mut tick = tick;
        tick.frame.sample.period = 2.0;
        assert_eq!(tick.snapshot(&parser, 9_000_000_000).unwrap().used, 700);
        assert_eq!(tick.snapshot(&parser, 9_000_000_001).unwrap().unknown, 1000);
        assert!(!report_fresh(2, 1, 1.0), "future receipts are not valid");
    }

    #[test]
    fn session_changes_clear_cached_readings_and_old_frames_cannot_restore_them() {
        let mut parser = Parser::default();
        let tick = tick();
        let key = ReporterKey::QubeMetrics(1);
        parser.parse(key, raw("a", 1_000_000_000, MEMORY));
        parser.parse(key, raw("b", 2_000_000_000, b"invalid"));
        parser.parse(key, raw("a", 1_000_000_000, MEMORY));
        assert_eq!(tick.snapshot(&parser, 2_000_000_000).unwrap().unknown, 1000);
        parser.parse(key, raw("b", 3_000_000_000, MEMORY));
        assert_eq!(tick.snapshot(&parser, 3_000_000_000).unwrap().used, 700);
        parser.parse(key, raw("disconnected", 2_500_000_000, b""));
        assert_eq!(tick.snapshot(&parser, 3_000_000_000).unwrap().used, 700);
        parser.parse(key, raw("disconnected", 4_000_000_000, b""));
        assert_eq!(tick.snapshot(&parser, 4_000_000_000).unwrap().unknown, 1000);
    }

    #[test]
    fn network_session_and_source_changes_clear_cached_rates_and_baselines() {
        let mut parser = Parser::default();
        let mut tick = tick();
        let key = ReporterKey::Network;
        let counters = b"N1 1500000000 12345678-1234-1234-1234-123456789abc 1:600:200";
        parser.parse(key, raw("a", 1_000_000_000, NETWORK));
        parser.parse(key, raw("a", 2_000_000_000, counters));
        assert_eq!(
            tick.snapshot(&parser, 2_000_000_000).unwrap().network,
            Some((1000.0, 0.0))
        );
        parser.parse(key, raw("b", 3_000_000_000, counters));
        assert!(tick
            .snapshot(&parser, 3_000_000_000)
            .unwrap()
            .network
            .is_none());
        tick.sources.insert(
            key,
            QubeIdentity {
                domain_id: 2,
                vm_path: "/vm/other".into(),
            },
        );
        parser.prune(&tick);
        assert!(!parser.reports.contains_key(&key));
    }
}
