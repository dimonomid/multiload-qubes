//! Keeps one guest reporting connection open and retries when it fails.
//!
//! Each reader owns a worker thread and a local qvm-run process group. The
//! sampling loop reserves requests; workers save raw replies without parsing
//! their measurements or making the sampling loop wait for guest I/O.
use super::diagnostics::StderrCapture;
use crate::metrics::handoff::{RawFrame, MAX_REPORT_BYTES};
use crate::metrics::publish_bytes;
use crate::metrics::{QubeIdentity, ReporterKey};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::signal::{killpg, Signal};
use nix::sys::wait::{waitid, Id, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
#[cfg(test)]
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// Maximum wait for an outstanding reply before reconnecting; idle connections have no timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
// Wait after a failed connection before trying again; shutdown interrupts this wait.
const RETRY_DELAY: Duration = Duration::from_secs(3);
// Check child exit this often after SIGTERM, during cleanup only. Live connections use poll().
const PROCESS_CHECK_INTERVAL: Duration = Duration::from_millis(20);
// Allow the direct child this long to exit after SIGTERM, then SIGKILL the group and reap it.
const PROCESS_EXIT_GRACE: Duration = Duration::from_secs(2);
// Refill the reply budget at this rate to reject sustained excessive stdout traffic.
const REPORTS_PER_SECOND: f64 = 4.0;
// Maximum saved reply budget, allowing a short burst before the rate limit disconnects.
const REPORT_BURST: f64 = 8.0;

pub const METRICS_SCRIPT: &str = include_str!("../../../guest/metrics.py");
pub const NETWORK_SCRIPT: &str = include_str!("../../../guest/network.py");

/// Quote the fixed Python source as one argument to the guest shell.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A worker's publication path and identity, both chosen before guest I/O.
#[derive(Clone)]
pub struct RawTarget {
    /// Latest raw frame file for this reporter.
    pub path: PathBuf,
    /// Qube identity attached to every reply from this connection.
    pub identity: QubeIdentity,
}
impl RawTarget {
    /// Attach collector context and save bytes without interpreting the guest measurements.
    pub fn publish(&self, tick_id: &str, session: &str, payload: &[u8]) -> Result<(), String> {
        if payload.len() > MAX_REPORT_BYTES {
            return Err("oversized raw report".into());
        }
        let frame = RawFrame {
            tick_id: tick_id.into(),
            identity: self.identity.clone(),
            session: session.into(),
            timestamp: multiload_qubes_snapshot::unix_time_now().map_err(|e| e.to_string())?,
            payload: payload.into(),
        };
        let bytes = serde_json::to_vec(&frame).map_err(|e| e.to_string())?;
        publish_bytes(&self.path, &bytes)
    }
}

/// Local command used to establish one persistent reporting connection.
#[derive(Clone)]
pub struct CommandSpec {
    /// Human-readable source in diagnostics, separate from executable arguments.
    pub source: String,
    /// Local executable, normally qvm-run.
    pub program: String,
    /// Arguments including the fixed guest script and validated qube name.
    pub args: Vec<String>,
}
impl CommandSpec {
    /// Open only the bundled reporter; do not allow guest data to choose a command.
    pub fn qube(name: &str, key: ReporterKey) -> Self {
        let script = match key {
            ReporterKey::QubeMetrics(_) => METRICS_SCRIPT,
            ReporterKey::Network => NETWORK_SCRIPT,
        };
        Self {
            source: format!(
                "{name} {}",
                match key {
                    ReporterKey::QubeMetrics(_) => "RAM",
                    ReporterKey::Network => "network",
                }
            ),
            program: "qvm-run".into(),
            args: vec![
                "--pass-io".into(),
                "--no-gui".into(),
                "--no-autostart".into(),
                name.into(),
                format!("python3 -u -c {}", shell_quote(script)),
            ],
        }
    }
}

/// Worker timeout and reconnect policy, separate from the parser's reply deadline.
#[derive(Clone, Copy)]
pub struct Timing {
    /// Maximum wait after sending a request; idle connections do not time out.
    pub stale: Duration,
    /// Delay before opening a new connection after a failure.
    pub retry: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            stale: REQUEST_TIMEOUT,
            retry: RETRY_DELAY,
        }
    }
}

// Signal the local qvm-run process group on error or stop. Descendants that
// leave the group are outside this cleanup. Bundled guest scripts exit when
// their stdin closes (or a write sees a broken pipe).
struct Process(Child);
impl Process {
    fn signal_group(&self, signal: Signal) {
        let group = Pid::from_raw(self.0.id() as i32);
        if let Err(error) = killpg(group, signal) {
            if error != Errno::ESRCH {
                eprintln!("Cannot send {signal} to guest process group {group}: {error}");
            }
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.signal_group(Signal::SIGTERM);
        let deadline = Instant::now() + PROCESS_EXIT_GRACE;
        loop {
            // Keep the parent unreaped so its PID/group ID cannot be reused
            // before we send the final signal to any remaining group members.
            match waitid(
                Id::Pid(Pid::from_raw(self.0.id() as i32)),
                WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
            ) {
                Ok(WaitStatus::StillAlive) | Err(Errno::EINTR) => {}
                Ok(_) => break,
                Err(Errno::ECHILD) => return, // The child is no longer ours to reap.
                Err(error) => {
                    eprintln!("Cannot check guest process {}: {error}", self.0.id());
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(PROCESS_CHECK_INTERVAL);
        }
        self.signal_group(Signal::SIGKILL);
        let _ = self.0.wait();
    }
}

/// Keep existing descriptor flags while making pipe I/O nonblocking.
fn set_nonblocking(fd: &impl AsFd) -> Result<(), Errno> {
    let flags = OFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFL)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(())
}

// The collector reserves an idle worker before listing it in a tick manifest.
// No lock is held during process I/O, and a busy worker never queues another tick.
#[derive(Default)]
enum RequestState {
    #[default]
    /// No connection is ready to accept a tick.
    Disconnected,
    /// Connected with no outstanding request.
    Idle,
    /// Reserved tick waiting for the worker to write sample to stdin.
    Queued(String),
    /// Worker has taken this tick; it cannot accept another until reply or disconnect.
    Pending(String),
}
/// Shared reservation slot between the sampling loop and one transport worker.
struct Requests {
    /// Actual work stays here; eventfd only tells the worker to check it.
    state: Mutex<RequestState>,
    /// Interrupts pipe and retry waits when a request or stop arrives.
    wake: EventFd,
}
impl Requests {
    fn new() -> io::Result<Self> {
        Ok(Self {
            state: Mutex::new(RequestState::default()),
            wake: EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)?,
        })
    }
    /// Update shared state before ringing. Several notifications may merge safely.
    fn notify(&self) {
        loop {
            match self.wake.write(1) {
                Ok(_) | Err(Errno::EAGAIN) => return,
                Err(Errno::EINTR) => continue,
                // The owned descriptor stays open while either thread can use it.
                Err(error) => panic!("cannot notify reporter worker: {error}"),
            }
        }
    }
    fn clear_notification(&self) -> Result<(), Errno> {
        loop {
            match self.wake.read() {
                Ok(_) | Err(Errno::EAGAIN) => return Ok(()),
                Err(Errno::EINTR) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    /// Claim an idle connection before adding it to the tick's expected replies.
    fn reserve(&self, tick_id: &str) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, RequestState::Idle) {
            *state = RequestState::Queued(tick_id.into());
            true
        } else {
            false
        }
    }
    fn ready(&self) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = RequestState::Idle;
    }
    /// Move a queued tick to pending before the worker writes its request.
    fn take(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let RequestState::Queued(tick_id) = &*state {
            let tick_id = tick_id.clone();
            *state = RequestState::Pending(tick_id.clone());
            Some(tick_id)
        } else {
            None
        }
    }
    /// Clear the reservation and return any tick that needs an unavailable reply.
    fn disconnect(&self) -> Option<String> {
        match std::mem::take(&mut *self.state.lock().unwrap_or_else(|e| e.into_inner())) {
            RequestState::Queued(tick_id) | RequestState::Pending(tick_id) => Some(tick_id),
            _ => None,
        }
    }
}

// Observe diagnostics in tests without sharing the worker's error state.
#[cfg(test)]
#[derive(Debug)]
enum LogEvent {
    Unavailable(String),
    Recovered,
}
/// Owns a reporting worker. Dropping it requests a stop and waits for the worker.
/// Call `stop()` on all readers first when shutting down several at once.
pub struct Reader {
    /// Single request slot; prevents queuing ticks behind a busy guest.
    requested: Arc<Requests>,
    /// Shared stop flag, checked after pipe or retry waits are interrupted.
    stop: Arc<AtomicBool>,
    #[cfg(test)]
    /// Test observations of normal diagnostics; absent from production builds.
    log_rx: Receiver<LogEvent>,
    /// Owned worker, joined on drop after requesting shutdown.
    thread: Option<JoinHandle<()>>,
}
impl Reader {
    /// Start a worker that owns reconnects, pipe I/O, and local process cleanup.
    pub fn spawn(spec: CommandSpec, timing: Timing, target: RawTarget) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let requested = Arc::new(Requests::new()?);
        let worker_requested = requested.clone();
        let worker_stop = stop.clone();
        #[cfg(test)]
        let (log_tx, log_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("guest-metrics".into())
            .spawn(move || {
                // Keep errors across reconnects; only a saved reply clears one.
                let mut last_error = None;
                while !worker_stop.load(Ordering::Relaxed) {
                    let result = session(
                        &spec,
                        timing,
                        &worker_stop,
                        &mut last_error,
                        &target,
                        &worker_requested,
                        #[cfg(test)]
                        &log_tx,
                    );
                    if let Some(tick_id) = worker_requested.disconnect() {
                        // A failed request is a terminal, unavailable reply.
                        let _ = target.publish(&tick_id, "disconnected", &[]);
                    }
                    if worker_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let reason = result.err().unwrap_or_else(|| "guest stream closed".into());
                    let deadline = Instant::now() + timing.retry;
                    if last_error.as_ref() != Some(&reason) {
                        eprintln!(
                            "Guest metrics [{}] unavailable: {}; retrying in {:.1}s",
                            spec.source,
                            reason,
                            timing.retry.as_secs_f64()
                        );
                        #[cfg(test)]
                        let _ = log_tx.send(LogEvent::Unavailable(reason.clone()));
                    }
                    last_error = Some(reason);
                    loop {
                        if let Err(error) = worker_requested.clear_notification() {
                            eprintln!("Cannot clear reporter notification: {error}");
                            return;
                        }
                        if worker_stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                            break;
                        }
                        let mut fds = [PollFd::new(
                            worker_requested.wake.as_fd(),
                            PollFlags::POLLIN,
                        )];
                        if let Err(error) = wait(&mut fds, Some(deadline)) {
                            eprintln!("Cannot wait to retry reporter: {error}");
                            return;
                        }
                    }
                }
            })?;
        Ok(Self {
            requested,
            stop,
            #[cfg(test)]
            log_rx,
            thread: Some(thread),
        })
    }
    /// Accept one tick only while connected and idle. Never wait for guest I/O.
    pub fn request(&self, tick_id: &str) -> bool {
        let accepted = !self.stop.load(Ordering::Relaxed) && self.requested.reserve(tick_id);
        if accepted {
            self.requested.notify();
        }
        accepted
    }
    /// Request shutdown without waiting; stop all readers before joining them.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.requested.notify();
    }
    pub fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Sleep until a descriptor is ready or the deadline expires. Signals do not extend the deadline.
fn wait(fds: &mut [PollFd<'_>], deadline: Option<Instant>) -> Result<(), Errno> {
    loop {
        // Round up to avoid spinning for the final fraction of a millisecond.
        let timeout = deadline.map_or(PollTimeout::NONE, |deadline| {
            let millis = deadline
                .saturating_duration_since(Instant::now())
                .as_nanos()
                .div_ceil(1_000_000)
                .min(i32::MAX as u128) as i32;
            PollTimeout::try_from(millis).unwrap()
        });
        match poll(fds, timeout) {
            Err(Errno::EINTR) => continue,
            result => return result.map(|_| ()),
        }
    }
}

/// Run one connection until stop or failure; only framing and traffic limits are checked here.
fn session(
    spec: &CommandSpec,
    timing: Timing,
    stop: &AtomicBool,
    last_error: &mut Option<String>,
    target: &RawTarget,
    requested: &Requests,
    #[cfg(test)] log_tx: &Sender<LogEvent>,
) -> Result<(), String> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut proc = Process(command.spawn().map_err(|e| e.to_string())?);
    // A reconnect must not reuse the network counter baseline from the old process.
    let session = format!(
        "{}-{}",
        proc.0.id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos()
    );
    let mut stdin = proc.0.stdin.take().ok_or("missing stdin")?;
    set_nonblocking(&stdin).map_err(|e| e.to_string())?;
    let mut stdout = proc.0.stdout.take().ok_or("missing stdout")?;
    set_nonblocking(&stdout).map_err(|e| e.to_string())?;
    let mut stderr = proc.0.stderr.take().ok_or("missing stderr")?;
    set_nonblocking(&stderr).map_err(|e| e.to_string())?;
    let mut captured = StderrCapture::default();
    requested.ready();
    let result = (|| -> Result<(), String> {
        let mut buffer = Vec::new();
        let mut chunk = [0; 4096];
        let mut pending: Option<(String, Instant)> = None;
        let mut budget_time = Instant::now();
        // Allow a short burst of buffered replies, but bound sustained traffic.
        // Guest output is untrusted even when replies were requested.
        let mut budget = REPORT_BURST;
        let mut stderr_open = true;
        loop {
            // Clear the doorbell before checking state. A later request/stop then
            // either appears in that check or keeps poll readable: no wakeup is lost.
            requested.clear_notification().map_err(|e| e.to_string())?;
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if stderr_open {
                stderr_open = captured
                    .drain(&mut stderr)
                    .map_err(|e| format!("cannot read guest stderr: {e}"))?;
            }
            if let Some(tick_id) = requested.take() {
                let request = b"sample\n";
                match stdin.write(request) {
                    Ok(n) if n == request.len() => pending = Some((tick_id, Instant::now())),
                    Ok(_) => return Err("incomplete sample request".into()),
                    Err(error) => return Err(format!("cannot request guest sample: {error}")),
                }
            }
            if pending
                .as_ref()
                .is_some_and(|(_, sent)| sent.elapsed() >= timing.stale)
            {
                return Err("reporter timed out".into());
            }
            match stdout.read(&mut chunk) {
                Ok(0) => return Err("guest stream closed".into()),
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    if buffer.len() > MAX_REPORT_BYTES {
                        return Err("oversized guest stream".into());
                    }
                    while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                        let now = Instant::now();
                        budget = (budget
                            + now.duration_since(budget_time).as_secs_f64() * REPORTS_PER_SECOND)
                            .min(REPORT_BURST)
                            - 1.0;
                        budget_time = now;
                        if budget < 0.0 {
                            return Err("guest exceeded sample rate limit".into());
                        }
                        let (tick_id, _) = pending.take().ok_or("unsolicited guest report")?;
                        target.publish(&tick_id, &session, &buffer[..end])?;
                        requested.ready();
                        buffer.drain(..=end);
                        // Only the restricted parser decides if metrics are valid.
                        if last_error.take().is_some() {
                            eprintln!("Guest metrics [{}] recovered", spec.source);
                            #[cfg(test)]
                            let _ = log_tx.send(LogEvent::Recovered);
                        }
                    }
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.to_string()),
            }
            let mut fds = [
                PollFd::new(requested.wake.as_fd(), PollFlags::POLLIN),
                PollFd::new(stdout.as_fd(), PollFlags::POLLIN),
                PollFd::new(stderr.as_fd(), PollFlags::POLLIN),
            ];
            // EOF stays readable forever. Stop watching closed stderr so an idle
            // connection does not spin after the guest closes that pipe alone.
            let count = if stderr_open { 3 } else { 2 };
            let deadline = pending.as_ref().map(|(_, sent)| *sent + timing.stale);
            wait(&mut fds[..count], deadline).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    result.map_err(|reason| captured.failure_details(&reason, &proc.0, &mut stderr, stop))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORTER: &str = "import sys\nfor command in sys.stdin:\n assert command == 'sample\\n'\n print('opaque reply',flush=True)";

    /// Exercise a requested failure and verify that its local child is reaped.
    fn session_error(script: &str, stale: Duration) -> String {
        let attempts = Attempts::new();
        let reader = attempts.start(
            &format!("import sys\nsys.stdin.readline()\n{script}"),
            Timing {
                stale,
                retry: Duration::from_secs(30),
            },
        );
        request_when_ready(&reader, "failure-tick");
        let reason = await_failure(&reader);
        let frame = attempts.frame("failure-tick");
        assert!(frame.payload.is_empty());
        drop(reader);
        attempts.assert_reaped();
        reason
    }

    #[test]
    fn reports_exit_status_and_sanitized_stderr() {
        let error = session_error(
            "import sys; sys.stderr.write('\\x1b[31mPermission denied\\x1b[0m\\n'); sys.exit(7)",
            Duration::from_secs(2),
        );
        assert!(error.contains("exit status 7"), "{error}");
        assert!(error.contains("stderr: Permission denied"), "{error}");
        assert!(error.is_ascii() && !error.chars().any(char::is_control));
    }

    #[test]
    fn reports_signal_exit_without_replacing_the_original_failure() {
        let error = session_error("import os,signal,sys; sys.stderr.write('crashed\\n'); sys.stderr.flush(); os.kill(os.getpid(),signal.SIGTERM)", Duration::from_secs(2));
        assert!(error.contains("signal SIGTERM"), "{error}");
        assert!(error.contains("stderr: crashed"), "{error}");

        let attempts = Attempts::new();
        let reader = attempts.start("import sys\nsys.stdin.readline()\nsys.stderr.write('script failed\\n'); sys.stderr.flush()\nprint('first\\nextra',flush=True)\nraise SystemExit(9)", Timing { retry: Duration::from_secs(30), ..timing() });
        request_when_ready(&reader, "test-tick");
        let error = await_failure(&reader);
        assert!(error.contains("unsolicited guest report"), "{error}");
        assert!(error.contains("exit status 9"), "{error}");
        assert!(error.contains("stderr: script failed"), "{error}");
        assert_eq!(attempts.frame("test-tick").payload, b"first");
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn stderr_limit_disconnects_even_when_the_guest_answers_requests() {
        let attempts = Attempts::new();
        let reader = attempts.start(
            "import sys\nfor command in sys.stdin:\n sys.stderr.write('x'*16384); sys.stderr.flush()\n print('opaque reply',flush=True)",
            Timing { stale: Duration::from_secs(3), retry: Duration::from_secs(30) },
        );
        for tick_id in ["one", "two", "three", "four"] {
            request_when_ready(&reader, tick_id);
            assert_eq!(attempts.frame(tick_id).payload, b"opaque reply");
        }
        request_when_ready(&reader, "over-limit");
        let error = await_failure(&reader);
        assert!(error.contains("stderr limit"), "{error}");
        assert!(error.contains("[truncated]"));
        assert!(!reader.request("during-backoff"));
        assert_eq!(attempts.pids().len(), 1);
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn stderr_only_flood_is_disconnected_before_request_timeout() {
        let start = Instant::now();
        let error = session_error(
            "import os; exec(\"while True: os.write(2,b'x'*4096)\")",
            Duration::from_secs(5),
        );
        assert!(error.contains("stderr limit"), "{error}");
        assert!(error.contains("[truncated]"));
        assert!(error.len() < 1300);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn closed_stdout_with_open_stderr_has_a_bounded_status_wait() {
        let start = Instant::now();
        let error = session_error("import os,sys; sys.stderr.write('stdout closed\\n'); sys.stderr.flush(); os.close(1); time.sleep(30)", Duration::from_secs(2));
        assert!(error.contains("guest stream closed"), "{error}");
        assert!(error.contains("exit status not yet available"), "{error}");
        assert!(error.contains("stderr: stdout closed"), "{error}");
        // Do not report our cleanup SIGTERM as the cause of the failure.
        assert!(!error.contains("SIGTERM"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    fn spawn_process(script: &str) -> Process {
        Process(
            Command::new("python3")
                .args(["-u", "-c", script])
                .stdout(Stdio::piped())
                .process_group(0)
                .spawn()
                .unwrap(),
        )
    }
    fn ready_line(proc: &mut Process) -> String {
        let mut stdout = proc.0.stdout.take().unwrap();
        set_nonblocking(&stdout).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut result = Vec::new();
        loop {
            let mut byte = [0];
            match stdout.read(&mut byte) {
                Ok(1) if byte[0] == b'\n' => return String::from_utf8(result).unwrap(),
                Ok(1) => result.push(byte[0]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                other => panic!("unexpected helper output: {other:?}"),
            }
            assert!(
                Instant::now() < deadline,
                "process helper did not become ready"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn nonblocking_pipe_preserves_flags_and_transfers_data() {
        let (read, write) = nix::unistd::pipe().unwrap();
        let before = OFlag::from_bits_retain(fcntl(&read, FcntlArg::F_GETFL).unwrap());
        set_nonblocking(&read).unwrap();
        let after = OFlag::from_bits_retain(fcntl(&read, FcntlArg::F_GETFL).unwrap());
        assert!(after.contains(OFlag::O_NONBLOCK));
        assert_eq!(after & !OFlag::O_NONBLOCK, before & !OFlag::O_NONBLOCK);
        let mut read = std::fs::File::from(read);
        let mut buf = [0];
        assert_eq!(
            read.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        nix::unistd::write(&write, b"x").unwrap();
        assert_eq!(read.read(&mut buf).unwrap(), 1);
        assert_eq!(buf, [b'x']);
    }
    #[test]
    fn process_cleanup_terminates_the_group_and_reaps_parent() {
        let mut proc = spawn_process("import subprocess, sys, time; child=subprocess.Popen([sys.executable,'-u','-c',\"import time; print('ready',flush=True); time.sleep(20)\"],stdout=subprocess.PIPE); child.stdout.readline(); print(child.pid,flush=True); time.sleep(20)");
        let parent = Pid::from_raw(proc.0.id() as i32);
        let child: u32 = ready_line(&mut proc).parse().unwrap();
        drop(proc);
        assert_eq!(nix::sys::signal::kill(parent, None), Err(Errno::ESRCH));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{child}/stat")).ok();
            // A terminated orphan can remain a zombie until its new parent reaps it.
            if stat.as_ref().is_none_or(|s| {
                s.split_once(") ")
                    .is_some_and(|(_, state)| state.starts_with('Z'))
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "descendant survived process-group cleanup"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn process_cleanup_kills_term_ignoring_descendant_after_parent_exits() {
        let mut proc = spawn_process("import subprocess,sys,time; child=subprocess.Popen([sys.executable,'-u','-c',\"import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); print('ready',flush=True); time.sleep(30)\"],stdout=subprocess.PIPE); child.stdout.readline(); print(child.pid,flush=True); time.sleep(30)");
        let parent = Pid::from_raw(proc.0.id() as i32);
        let child: u32 = ready_line(&mut proc).parse().unwrap();
        drop(proc);
        assert_eq!(nix::sys::signal::kill(parent, None), Err(Errno::ESRCH));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{child}/stat")).ok();
            if stat.as_ref().is_none_or(|s| {
                s.split_once(") ")
                    .is_some_and(|(_, state)| state.starts_with('Z'))
            }) {
                break;
            }
            if Instant::now() >= deadline {
                // A failing regression test must not leave the helper running.
                let _ = nix::sys::signal::kill(Pid::from_raw(child as i32), Signal::SIGKILL);
                panic!("SIGTERM-ignoring descendant survived its parent's cleanup");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn process_cleanup_escalates_when_term_is_ignored() {
        let mut proc = spawn_process("import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); print('ready',flush=True); time.sleep(20)");
        let pid = Pid::from_raw(proc.0.id() as i32);
        assert_eq!(ready_line(&mut proc), "ready");
        let start = Instant::now();
        drop(proc);
        assert!(start.elapsed() >= Duration::from_secs(2));
        assert!(start.elapsed() < Duration::from_secs(4));
        assert_eq!(nix::sys::signal::kill(pid, None), Err(Errno::ESRCH));
    }
    fn spec(script: &str) -> CommandSpec {
        CommandSpec {
            source: "test RAM".into(),
            program: "python3".into(),
            args: vec!["-u".into(), "-c".into(), script.into()],
        }
    }
    fn timing() -> Timing {
        Timing {
            stale: Duration::from_millis(300),
            retry: Duration::from_millis(40),
        }
    }
    /// Temporary raw-output directory and PID log for real local reporter processes.
    struct Attempts(std::path::PathBuf);
    impl Attempts {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "mq-reader-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn script(&self, body: &str) -> String {
            // Each attempt records its PID before doing any reporting or failing.
            format!(
                "import os,time,pathlib\np=pathlib.Path({:?})/'pids'\nwith p.open('a') as f: f.write(str(os.getpid())+'\\n')\n{body}",
                self.0.to_str().unwrap()
            )
        }
        fn pids(&self) -> Vec<Pid> {
            std::fs::read_to_string(self.0.join("pids"))
                .unwrap_or_default()
                .lines()
                .map(|line| Pid::from_raw(line.parse().unwrap()))
                .collect()
        }
        fn assert_reaped(&self) {
            let pids = self.pids();
            assert!(!pids.is_empty(), "helper never started");
            for pid in pids {
                assert_eq!(
                    nix::sys::signal::kill(pid, None),
                    Err(Errno::ESRCH),
                    "helper {pid} survived or was not reaped"
                );
            }
        }
    }
    impl Drop for Attempts {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Attempts {
        fn target(&self) -> RawTarget {
            RawTarget {
                path: self.0.join("raw"),
                identity: QubeIdentity {
                    domain_id: 23,
                    vm_path: "test".into(),
                },
            }
        }
        fn start(&self, body: &str, timing: Timing) -> Reader {
            Reader::spawn(spec(&self.script(body)), timing, self.target()).unwrap()
        }
        /// Wait for a publication belonging to this tick, ignoring the previous file.
        fn frame(&self, tick_id: &str) -> RawFrame {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if let Ok(bytes) = std::fs::read(self.0.join("raw")) {
                    let frame: RawFrame = serde_json::from_slice(&bytes).unwrap();
                    if frame.tick_id == tick_id {
                        return frame;
                    }
                }
                assert!(Instant::now() < deadline, "no raw reply for {tick_id}");
                thread::sleep(Duration::from_millis(5));
            }
        }
        fn wait_started(&self) {
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.pids().is_empty() {
                assert!(Instant::now() < deadline, "helper never started");
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn next_log(reader: &Reader) -> LogEvent {
        reader
            .log_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("worker did not report a diagnostic")
    }
    fn await_failure(reader: &Reader) -> String {
        match next_log(reader) {
            LogEvent::Unavailable(reason) => reason,
            other => panic!("expected a failure diagnostic, got {other:?}"),
        }
    }

    /// Wait for a real connection reservation rather than assuming a startup delay.
    fn request_when_ready(reader: &Reader, tick_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !reader.request(tick_id) {
            assert!(Instant::now() < deadline, "worker did not become idle");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn raw_frames_keep_bytes_identity_and_session_across_requests() {
        let attempts = Attempts::new();
        let reader = attempts.start("import sys\nfor command in sys.stdin:\n sys.stdout.buffer.write(b'M1 24 invalid: \\xff\\x00\\n'); sys.stdout.flush()", timing());
        request_when_ready(&reader, "first-tick");
        let first = attempts.frame("first-tick");
        // Wrong-domain and malformed measurement fields belong to parser tests.
        assert_eq!(first.payload, b"M1 24 invalid: \xff\x00");
        assert_eq!(
            first.identity,
            QubeIdentity {
                domain_id: 23,
                vm_path: "test".into()
            }
        );
        assert!(!first.session.is_empty());
        assert!(first.timestamp > 0.0);
        request_when_ready(&reader, "second-tick");
        let second = attempts.frame("second-tick");
        assert_eq!(second.payload, first.payload);
        assert_eq!(second.session, first.session);
        assert!(second.timestamp >= first.timestamp);
        assert_eq!(attempts.pids().len(), 1);
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn partial_lines_wait_for_a_complete_reply() {
        let attempts = Attempts::new();
        let reader = attempts.start("import sys\nsys.stdin.readline()\nsys.stdout.write('part'); sys.stdout.flush()\ntime.sleep(.05)\nprint('ial',flush=True)\ntime.sleep(30)", timing());
        request_when_ready(&reader, "test-tick");
        assert_eq!(attempts.frame("test-tick").payload, b"partial");
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn unexpected_eof_reports_failure_and_waits_before_retry() {
        let attempts = Attempts::new();
        let reader = attempts.start(
            "pass",
            Timing {
                retry: Duration::from_secs(30),
                ..timing()
            },
        );
        let reason = await_failure(&reader);
        assert!(reason.starts_with("guest stream closed"), "{reason}");
        assert!(reason.contains("exit status 0"), "{reason}");
        thread::sleep(Duration::from_millis(50));
        assert_eq!(attempts.pids().len(), 1);
        assert!(!reader.request("unavailable-tick"));
        assert!(!attempts.target().path.exists());
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn rejects_unsolicited_and_oversized_output() {
        let error = session_error(
            &format!(
                "print('x'*{},flush=True)\ntime.sleep(30)",
                MAX_REPORT_BYTES + 1
            ),
            Duration::from_secs(2),
        );
        assert!(error.contains("oversized guest stream"), "{error}");

        let attempts = Attempts::new();
        let reader = attempts.start(
            "print('unrequested reply',flush=True)\ntime.sleep(30)",
            Timing {
                retry: Duration::from_secs(30),
                ..timing()
            },
        );
        let reason = await_failure(&reader);
        assert!(reason.contains("unsolicited guest report"), "{reason}");
        assert!(!attempts.target().path.exists());
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn requested_replies_still_obey_the_rate_limit() {
        let attempts = Attempts::new();
        let reader = attempts.start(
            REPORTER,
            Timing {
                stale: Duration::from_secs(2),
                retry: Duration::from_secs(30),
            },
        );
        for i in 0..32 {
            let tick_id = format!("tick-{i}");
            request_when_ready(&reader, &tick_id);
            let frame = attempts.frame(&tick_id);
            if frame.payload.is_empty() {
                let reason = await_failure(&reader);
                assert!(reason.contains("sample rate limit"), "{reason}");
                drop(reader);
                attempts.assert_reaped();
                return;
            }
            assert_eq!(frame.payload, b"opaque reply");
        }
        panic!("rapid requests did not hit the sample rate limit");
    }
    #[test]
    fn notifications_survive_until_poll_and_merge_without_losing_requests() {
        let requests = Requests::new().unwrap();
        requests.ready();
        assert!(requests.reserve("tick-1"));
        requests.notify();
        requests.notify();
        let mut fds = [PollFd::new(requests.wake.as_fd(), PollFlags::POLLIN)];
        // A notification sent before poll must remain readable.
        assert_eq!(poll(&mut fds, PollTimeout::ZERO).unwrap(), 1);
        requests.clear_notification().unwrap();
        assert_eq!(poll(&mut fds, PollTimeout::ZERO).unwrap(), 0);
        assert_eq!(requests.take().as_deref(), Some("tick-1"));

        let requests = Arc::new(requests);
        let worker_requests = requests.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut fds = [PollFd::new(worker_requests.wake.as_fd(), PollFlags::POLLIN)];
            started_tx.send(()).unwrap();
            wait(&mut fds, None).unwrap();
            done_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(80)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        requests.notify();
        done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn idle_connection_handles_closed_stderr_and_wakes_for_requests() {
        let attempts = Attempts::new();
        let reader = attempts.start(
            &format!("import os\nos.close(2)\n{REPORTER}"),
            Timing {
                stale: Duration::from_secs(3),
                ..timing()
            },
        );
        attempts.wait_started();
        // Let both pipes settle and the worker enter its idle wait.
        thread::sleep(Duration::from_millis(200));
        for tick_id in ["first", "second"] {
            request_when_ready(&reader, tick_id);
            assert_eq!(attempts.frame(tick_id).payload, b"opaque reply");
            thread::sleep(Duration::from_millis(200));
        }
        assert_eq!(attempts.pids().len(), 1);
        let stopped = Instant::now();
        drop(reader);
        assert!(stopped.elapsed() < Duration::from_secs(1));
        attempts.assert_reaped();
    }

    #[test]
    fn request_slots_keep_tick_ids_and_skip_busy_connections() {
        let requests = Requests::new().unwrap();
        assert!(!requests.reserve("tick-1"));
        requests.ready();
        assert!(requests.reserve("tick-1"));
        assert!(!requests.reserve("tick-2"));
        assert_eq!(requests.take().as_deref(), Some("tick-1"));
        assert!(!requests.reserve("tick-2"));
        assert_eq!(requests.disconnect().as_deref(), Some("tick-1"));
        assert!(!requests.reserve("tick-3"));
        requests.ready();
        assert!(requests.reserve("tick-3"));
        assert_eq!(requests.take().as_deref(), Some("tick-3"));
    }

    #[test]
    fn requests_are_tick_driven_and_do_not_queue_behind_a_slow_reply() {
        let attempts = Attempts::new();
        let requests = attempts.0.join("requests");
        let gate = attempts.0.join("reply");
        let target = attempts.0.join("raw");
        let body = format!("import sys\nrequests=pathlib.Path({requests:?})\ngate=pathlib.Path({gate:?})\nfor command in sys.stdin:\n with requests.open('a') as f: f.write(command)\n while not gate.exists(): time.sleep(.005)\n print('M1 23 1000 400',flush=True)");
        let reader = Reader::spawn(
            spec(&attempts.script(&body)),
            Timing {
                stale: Duration::from_millis(250),
                retry: Duration::from_secs(5),
            },
            RawTarget {
                path: target.clone(),
                identity: QubeIdentity {
                    domain_id: 23,
                    vm_path: "test".into(),
                },
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while attempts.pids().is_empty() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        // Idle between collector ticks is not a timed-out request.
        thread::sleep(Duration::from_millis(300));
        assert!(!requests.exists());
        assert!(matches!(
            reader.log_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        request_when_ready(&reader, "test-tick");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !requests.exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..10 {
            assert!(!reader.request("skipped-tick"));
            thread::sleep(Duration::from_millis(2));
        }
        thread::sleep(Duration::from_millis(40));
        assert_eq!(std::fs::read_to_string(&requests).unwrap(), "sample\n");
        std::fs::write(gate, b"reply now").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !target.exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let frame: RawFrame = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(frame.payload, b"M1 23 1000 400");
        assert_eq!(frame.tick_id, "test-tick");
        thread::sleep(Duration::from_millis(40));
        assert_eq!(std::fs::read_to_string(&requests).unwrap(), "sample\n");
        request_when_ready(&reader, "next-tick");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let frame: RawFrame = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
            if frame.tick_id == "next-tick" {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn unanswered_requests_time_out_and_are_reaped() {
        let attempts = Attempts::new();
        let reader = Reader::spawn(
            spec(&attempts.script("import sys\nsys.stdin.readline()\ntime.sleep(30)")),
            Timing {
                stale: Duration::from_millis(100),
                retry: Duration::from_secs(5),
            },
            RawTarget {
                path: attempts.0.join("raw"),
                identity: QubeIdentity {
                    domain_id: 23,
                    vm_path: "test".into(),
                },
            },
        )
        .unwrap();
        request_when_ready(&reader, "test-tick");
        let reason = await_failure(&reader);
        assert!(reason.starts_with("reporter timed out"), "{reason}");
        let frame: RawFrame =
            serde_json::from_slice(&std::fs::read(attempts.0.join("raw")).unwrap()).unwrap();
        assert_eq!(frame.tick_id, "test-tick");
        assert!(frame.payload.is_empty());
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn timeout_reconnects_with_a_new_session_and_keeps_tick_ids() {
        let attempts = Attempts::new();
        let reader = attempts.start("import sys\nsys.stdin.readline()\nprint('opaque reply',flush=True)\nsys.stdin.readline()\ntime.sleep(30)", Timing { stale: Duration::from_millis(100), ..timing() });
        request_when_ready(&reader, "first-tick");
        let first = attempts.frame("first-tick");
        request_when_ready(&reader, "missed-tick");
        let missed = attempts.frame("missed-tick");
        assert!(missed.payload.is_empty());
        assert_eq!(missed.session, "disconnected");
        request_when_ready(&reader, "recovered-tick");
        let recovered = attempts.frame("recovered-tick");
        assert_eq!(recovered.payload, first.payload);
        assert_ne!(recovered.session, first.session);
        drop(reader);
        assert!(attempts.pids().len() >= 2);
        attempts.assert_reaped();
    }

    #[test]
    fn repeated_failed_reconnects_are_reaped_and_eventually_recover() {
        let attempts = Attempts::new();
        let reader = attempts.start(
            "if len(p.read_text().splitlines()) <= 3: raise SystemExit(7)\nimport sys\nsys.stdin.readline()\nprint('opaque reply',flush=True)\nsys.stdin.readline()\nraise SystemExit(7)",
            Timing {
                stale: Duration::from_secs(30),
                retry: Duration::from_millis(200),
            },
        );
        let first_error = await_failure(&reader);
        assert!(!reader.request("failed-tick"));
        attempts.assert_reaped();
        // A failed attempt must wait for its retry deadline, not spin spawning.
        let count = attempts.pids().len();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(attempts.pids().len(), count);
        // A new process can accept a request before its startup failure is known.
        // Wait for the successful attempt before asking it for a reply.
        let deadline = Instant::now() + Duration::from_secs(3);
        while attempts.pids().len() < 4 {
            assert!(Instant::now() < deadline, "reconnect attempts stopped");
            thread::sleep(Duration::from_millis(5));
        }
        request_when_ready(&reader, "recovered-tick");
        assert_eq!(attempts.frame("recovered-tick").payload, b"opaque reply");
        assert_eq!(attempts.pids().len(), 4);
        // Repeated identical failures produced no extra notices before recovery.
        assert!(matches!(next_log(&reader), LogEvent::Recovered));
        request_when_ready(&reader, "failed-again");
        assert_eq!(await_failure(&reader), first_error);
        assert!(attempts.frame("failed-again").payload.is_empty());
        let start = Instant::now();
        drop(reader);
        assert!(start.elapsed() < Duration::from_secs(1));
        attempts.assert_reaped();
    }

    #[test]
    fn changed_failures_are_logged_without_a_successful_reply() {
        let attempts = Attempts::new();
        let reader = attempts.start(
            "raise SystemExit(7 if len(p.read_text().splitlines()) == 1 else 9)",
            Timing {
                retry: Duration::from_millis(200),
                ..timing()
            },
        );
        assert!(await_failure(&reader).contains("exit status 7"));
        assert!(await_failure(&reader).contains("exit status 9"));
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn failed_spawn_can_recover_and_stop_during_retry() {
        let attempts = Attempts::new();
        let program = attempts.0.join("reporter");
        let mut command = spec(&attempts.script(REPORTER));
        command.program = program.to_str().unwrap().into();
        let reader = Reader::spawn(
            command.clone(),
            Timing {
                stale: Duration::from_secs(30),
                retry: Duration::from_millis(100),
            },
            attempts.target(),
        )
        .unwrap();
        await_failure(&reader);
        assert!(!reader.request("failed-tick"));
        assert!(attempts.pids().is_empty());
        std::os::unix::fs::symlink("/usr/bin/python3", &program).unwrap();
        request_when_ready(&reader, "recovered-tick");
        assert_eq!(attempts.frame("recovered-tick").payload, b"opaque reply");
        drop(reader);
        attempts.assert_reaped();

        std::fs::remove_file(program).unwrap();
        let reader = Reader::spawn(
            command,
            Timing {
                retry: Duration::from_secs(30),
                ..timing()
            },
            attempts.target(),
        )
        .unwrap();
        await_failure(&reader);
        let start = Instant::now();
        drop(reader);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(attempts.pids().len(), 1);
    }

    #[test]
    fn shutdown_interrupts_idle_pending_and_replied_sessions_and_reaps_children() {
        for mode in ["idle", "pending", "replied"] {
            let attempts = Attempts::new();
            let body = match mode {
                "replied" => REPORTER,
                "pending" => "import sys\nsys.stdin.readline()\np.with_name('requested').touch()\ntime.sleep(30)",
                _ => "time.sleep(30)",
            };
            let reader = attempts.start(
                body,
                Timing {
                    stale: Duration::from_secs(30),
                    retry: Duration::from_secs(30),
                },
            );
            attempts.wait_started();
            if mode != "idle" {
                request_when_ready(&reader, "test-tick");
            }
            if mode == "pending" {
                let deadline = Instant::now() + Duration::from_secs(3);
                while !attempts.0.join("requested").exists() {
                    assert!(Instant::now() < deadline, "request was not delivered");
                    thread::sleep(Duration::from_millis(5));
                }
            }
            if mode == "replied" {
                assert_eq!(attempts.frame("test-tick").payload, b"opaque reply");
            }
            let start = Instant::now();
            reader.stop();
            assert!(!reader.request("stopped-tick"));
            while !reader.finished() {
                assert!(
                    start.elapsed() < Duration::from_secs(1),
                    "worker did not stop"
                );
                thread::sleep(Duration::from_millis(5));
            }
            drop(reader);
            attempts.assert_reaped();
        }
    }

    #[test]
    fn shutdown_interrupts_long_retry_wait_without_another_attempt() {
        let attempts = Attempts::new();
        let reader = attempts.start(
            "raise SystemExit(7)",
            Timing {
                retry: Duration::from_secs(30),
                ..timing()
            },
        );
        await_failure(&reader);
        attempts.assert_reaped();
        let start = Instant::now();
        drop(reader);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(attempts.pids().len(), 1);
        attempts.assert_reaped();
    }
    #[test]
    fn qubes_command_never_autostarts() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        let cmd = CommandSpec::qube("dev", ReporterKey::QubeMetrics(23));
        assert_eq!(cmd.program, "qvm-run");
        assert!(cmd.args.iter().any(|s| s == "--no-autostart"));
        assert!(cmd.args.iter().any(|s| s == "--no-gui"));
        assert!(cmd.args.last().unwrap().contains("xenstore-read"));
        assert_eq!(cmd.source, "dev RAM");
        assert_eq!(
            CommandSpec::qube("sys-net", ReporterKey::Network).source,
            "sys-net network"
        );
    }
}
