//! Follow atomic metrics publications. The only live-data timer detects silence.
use super::graphs::{Graphs, InputVersion};
use crate::Snapshot;
use glib::SourceId;
use nix::errno::Errno;
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor};
use std::cell::{Cell, RefCell};
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Watch the metrics directory and its parent so service restarts do not lose the watch.
struct Watcher {
    /// Nonblocking descriptor integrated into the GTK main loop.
    inotify: Inotify,
    /// Watch for removal or recreation of the service runtime directory.
    parent: WatchDescriptor,
    /// Current metrics-directory watch, absent while that directory is missing.
    directory: Cell<Option<WatchDescriptor>>,
    /// Snapshot filename to filter from directory events.
    path: PathBuf,
}
impl Watcher {
    fn new(path: &Path) -> Result<Self, String> {
        let inotify = Inotify::init(InitFlags::IN_CLOEXEC | InitFlags::IN_NONBLOCK)
            .map_err(|e| e.to_string())?;
        let directory = path.parent().ok_or("metrics path has no directory")?;
        let parent = directory
            .parent()
            .ok_or("metrics directory has no parent")?;
        let parent = inotify
            .add_watch(
                parent,
                AddWatchFlags::IN_CREATE
                    | AddWatchFlags::IN_DELETE
                    | AddWatchFlags::IN_MOVED_TO
                    | AddWatchFlags::IN_MOVED_FROM,
            )
            .map_err(|e| e.to_string())?;
        let watcher = Self {
            inotify,
            parent,
            directory: Cell::new(None),
            path: path.into(),
        };
        watcher.attach();
        Ok(watcher)
    }
    /// Replace the directory watch; a missing runtime directory is expected while stopped.
    fn attach(&self) {
        if let Some(old) = self.directory.take() {
            let _ = self.inotify.rm_watch(old);
        }
        let flags = AddWatchFlags::IN_MOVED_TO
            | AddWatchFlags::IN_DELETE
            | AddWatchFlags::IN_DELETE_SELF
            | AddWatchFlags::IN_MOVE_SELF;
        self.directory.set(
            self.inotify
                .add_watch(self.path.parent().unwrap(), flags)
                .ok(),
        );
    }
    /// Drain notifications and report whether the metrics file may need a read.
    fn changed(&self) -> Result<bool, String> {
        let events = match self.inotify.read_events() {
            Ok(events) => events,
            Err(Errno::EAGAIN) | Err(Errno::EINTR) => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        let mut changed = false;
        for event in events {
            if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                self.attach();
                changed = true;
            } else if event.wd == self.parent
                && event.name.as_deref() == self.path.parent().and_then(Path::file_name)
            {
                // Services may remove and recreate their runtime directory.
                self.attach();
                changed = true;
            } else if Some(event.wd) == self.directory.get() {
                if event.mask.intersects(
                    AddWatchFlags::IN_DELETE_SELF
                        | AddWatchFlags::IN_MOVE_SELF
                        | AddWatchFlags::IN_IGNORED,
                ) {
                    self.directory.set(None);
                    self.attach();
                    changed = true;
                } else if event.name.as_deref() == self.path.file_name() {
                    changed = true;
                }
            }
        }
        Ok(changed)
    }
}

/// Learn publication spacing and allow twice that interval before drawing a gap.
struct Cadence {
    /// Last live arrival; cleared during downtime so recovery does not learn a long gap.
    last: Option<Instant>,
    /// Last useful interval, retained while the service is silent.
    interval: Duration,
    /// Collector period from the last sample, used to detect configuration changes.
    advertised: Option<Duration>,
}
impl Default for Cadence {
    fn default() -> Self {
        Self {
            last: None,
            interval: Duration::from_secs(1),
            advertised: None,
        }
    }
}
impl Cadence {
    /// Learn arrival spacing without treating downtime or compressed catch-up as the cadence.
    fn received(&mut self, now: Instant, period: f64) {
        // Seconds match the snapshot file; convert to Duration here because this code sets timers.
        let advertised = Duration::from_secs_f64(period);
        // Use the collector's interval until two arrivals can be compared, and
        // after a restart or interval change. Downtime is not a sampling interval.
        self.interval = if self.advertised == Some(advertised) {
            self.last
                .and_then(|last| now.checked_duration_since(last))
                // A delayed main loop can observe catch-up publications almost
                // together. That gap is not the collector's sampling cadence.
                .filter(|d| *d >= advertised / 2)
                .unwrap_or(advertised)
        } else {
            advertised
        };
        self.last = Some(now);
        self.advertised = Some(advertised);
    }
    fn timeout(&self) -> Duration {
        self.interval.saturating_mul(2)
    }
    /// Forget the last arrival while retaining the interval for further gap columns.
    fn missing(&mut self) {
        self.last = None;
    }
}

/// GTK-thread subscription: new files add samples; silence adds missing-data columns.
pub(super) struct Feed {
    /// Directory watches used to follow atomic replacements.
    watcher: Watcher,
    /// GLib descriptor callback, removed on drop.
    source: RefCell<Option<SourceId>>,
    /// One silence timer, replaced on each accepted publication.
    timer: RefCell<Option<SourceId>>,
    /// Observed spacing and timeout policy.
    cadence: RefCell<Cadence>,
    /// Last file already delivered; duplicate notifications do not add columns.
    version: Cell<Option<InputVersion>>,
    /// Receives a fresh snapshot or None for a missing-data column.
    callback: Box<dyn Fn(Option<Snapshot>)>,
}
impl Feed {
    /// Install watches before reading the initial file, then arm the silence timer.
    pub fn start(
        path: &Path,
        callback: impl Fn(Option<Snapshot>) + 'static,
    ) -> Result<Rc<Self>, String> {
        let feed = Rc::new(Self {
            watcher: Watcher::new(path)?,
            source: RefCell::new(None),
            timer: RefCell::new(None),
            cadence: RefCell::new(Cadence::default()),
            version: Cell::new(None),
            callback: Box::new(callback),
        });
        let weak = Rc::downgrade(&feed);
        let source = glib::source::unix_fd_add_local(
            feed.watcher.inotify.as_fd().as_raw_fd(),
            glib::IOCondition::IN,
            move |_, _| {
                if let Some(feed) = weak.upgrade() {
                    match feed.watcher.changed() {
                        Ok(true) => {
                            feed.read();
                        }
                        Ok(false) => {}
                        Err(error) => eprintln!("Cannot watch Multiload metrics: {error}"),
                    }
                    glib::ControlFlow::Continue
                } else {
                    glib::ControlFlow::Break
                }
            },
        );
        *feed.source.borrow_mut() = Some(source);
        if !feed.read() {
            (feed.callback)(None);
            feed.arm(Duration::from_secs(2));
        } else {
            // The initial file may already be old. Measure intervals only
            // between subsequent notifications, not from this initial read.
            feed.cadence.borrow_mut().last = None;
        }
        Ok(feed)
    }
    /// Deliver only a fresh, valid file version and reset the silence timer.
    fn read(self: &Rc<Self>) -> bool {
        let Some((sample, version)) = Graphs::read_snapshot(&self.watcher.path) else {
            return false;
        };
        // Overflow recovery and directory notifications can refer to the same file.
        if self.version.get() == Some(version) {
            return false;
        }
        self.version.set(Some(version));
        self.cadence
            .borrow_mut()
            .received(Instant::now(), sample.period);
        (self.callback)(Some(sample));
        let timeout = self.cadence.borrow().timeout();
        self.arm(timeout);
        true
    }
    /// Replace the one-shot timer; its callback uses a weak owner to avoid a reference cycle.
    fn arm(self: &Rc<Self>, delay: Duration) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(delay.max(Duration::from_millis(1)), move || {
            if let Some(feed) = weak.upgrade() {
                // This source is already firing; do not remove its ID again.
                feed.timer.borrow_mut().take();
                feed.cadence.borrow_mut().missing();
                (feed.callback)(None);
                let timeout = feed.cadence.borrow().timeout();
                feed.arm(timeout);
            }
        });
        *self.timer.borrow_mut() = Some(timer);
    }
}
impl Drop for Feed {
    fn drop(&mut self) {
        if let Some(source) = self.source.get_mut().take() {
            source.remove();
        }
        if let Some(timer) = self.timer.get_mut().take() {
            timer.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Temporary runtime directory that tests may delete and recreate under a live watch.
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "mq-panel-watch-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn metrics(&self) -> PathBuf {
            self.0.join("runtime/metrics")
        }
        fn publish(&self) {
            fs::create_dir_all(self.0.join("runtime")).unwrap();
            let now = multiload_qubes_snapshot::unix_time_now().unwrap();
            crate::metrics::publish(
                &self.metrics(),
                &format!("MQ6 {now:.6} 25 4000 2000 100 1000 0 0.25 unknown unknown unknown - unknown unknown unknown unknown - unknown unknown unknown -\n"),
            )
            .unwrap();
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cadence_uses_arrivals_without_learning_downtime_as_an_interval() {
        let mut cadence = Cadence::default();
        assert_eq!(cadence.timeout(), Duration::from_secs(2));
        let now = Instant::now();
        cadence.received(now, 0.5);
        assert_eq!(cadence.timeout(), Duration::from_secs(1));
        cadence.received(now + Duration::from_millis(600), 0.5);
        assert_eq!(cadence.timeout(), Duration::from_millis(1200));
        cadence.missing();
        assert_eq!(cadence.interval, Duration::from_millis(600));
        cadence.received(now + Duration::from_secs(60), 0.5);
        assert_eq!(cadence.timeout(), Duration::from_secs(1));
        cadence.received(
            now + Duration::from_secs(60) + Duration::from_micros(1),
            0.5,
        );
        assert_eq!(cadence.timeout(), Duration::from_secs(1));
        cadence.received(now + Duration::from_millis(60500), 10.0);
        assert_eq!(cadence.timeout(), Duration::from_secs(20));
    }

    #[test]
    fn watcher_follows_atomic_updates_and_runtime_directory_recreation() {
        let dir = Directory::new();
        let watcher = Watcher::new(&dir.metrics()).unwrap();
        dir.publish();
        assert!(watcher.changed().unwrap());
        dir.publish();
        assert!(watcher.changed().unwrap());
        assert!(!watcher.changed().unwrap());
        fs::remove_dir_all(dir.0.join("runtime")).unwrap();
        assert!(watcher.changed().unwrap());
        dir.publish();
        assert!(watcher.changed().unwrap());
        dir.publish();
        assert!(watcher.changed().unwrap());
    }

    fn wait(context: &glib::MainContext, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            context.iteration(false);
            assert!(
                Instant::now() < deadline,
                "panel feed did not reach expected state"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn feed_updates_without_polling_generates_gaps_recovers_and_cleans_up() {
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let dir = Directory::new();
        let samples = Rc::new(RefCell::new(Vec::new()));
        let received = samples.clone();
        let feed = Feed::start(&dir.metrics(), move |sample| {
            received.borrow_mut().push(sample.is_some())
        })
        .unwrap();
        assert_eq!(*samples.borrow(), vec![false]);
        dir.publish();
        wait(&context, || samples.borrow().last() == Some(&true));
        let count = samples.borrow().len();
        assert!(
            !feed.read(),
            "the same publication must not advance history twice"
        );
        assert_eq!(samples.borrow().len(), count);
        // A malformed replacement must not count as a valid heartbeat.
        crate::metrics::publish(&dir.metrics(), "invalid snapshot").unwrap();
        wait(&context, || samples.borrow().len() > count);
        assert_eq!(samples.borrow().last(), Some(&false));
        let count = samples.borrow().len();
        let first_gap = Instant::now();
        wait(&context, || samples.borrow().len() > count);
        assert!(
            first_gap.elapsed() >= Duration::from_millis(450),
            "subsequent gaps must also wait twice the 250 ms interval"
        );
        assert_eq!(samples.borrow().last(), Some(&false));
        fs::remove_dir_all(dir.0.join("runtime")).unwrap();
        context.iteration(false);
        dir.publish();
        wait(&context, || samples.borrow().last() == Some(&true));
        let weak = Rc::downgrade(&feed);
        drop(feed);
        assert!(weak.upgrade().is_none());
        let count = samples.borrow().len();
        dir.publish();
        let end = Instant::now() + Duration::from_millis(600);
        while Instant::now() < end {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            samples.borrow().len(),
            count,
            "destroyed panels must have no live sources"
        );
    }
}
