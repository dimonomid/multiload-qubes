//! Keeps one guest reporting connection open and retries when it fails.
//!
//! Each reader owns a worker thread and a local qvm-run process group. The
//! sampling loop reserves requests; workers save raw replies without parsing
//! their measurements or making the sampling loop wait for guest I/O.
use super::diagnostics::StderrCapture;
use crate::metrics::handoff::{RawFrame, MAX_REPORT_BYTES};
use crate::metrics::protocol::Kind;
use crate::metrics::publish_bytes;
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::signal::{killpg, Signal};
use nix::sys::wait::{waitid, Id, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_DELAY: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const PROCESS_EXIT_GRACE: Duration = Duration::from_secs(2);
const REPORTS_PER_SECOND: f64 = 4.0;
const REPORT_BURST: f64 = 8.0;

pub const METRICS_SCRIPT: &str = include_str!("../../guest/metrics.py");
pub const NETWORK_SCRIPT: &str = include_str!("../../guest/network.py");

pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[derive(Clone)]
pub struct RawTarget {
    pub path: PathBuf,
    pub identity: String,
}
impl RawTarget {
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

#[derive(Clone)]
pub struct CommandSpec {
    pub source: String,
    pub program: String,
    pub args: Vec<String>,
}
impl CommandSpec {
    pub fn qube(name: &str, kind: Kind) -> Self {
        let script = match kind {
            Kind::Memory(_) => METRICS_SCRIPT,
            Kind::Network => NETWORK_SCRIPT,
        };
        Self {
            source: format!(
                "{name} {}",
                match kind {
                    Kind::Memory(_) => "RAM",
                    Kind::Network => "network",
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

#[derive(Clone, Copy)]
pub struct Timing {
    pub stale: Duration,
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
            thread::sleep(POLL_INTERVAL);
        }
        self.signal_group(Signal::SIGKILL);
        let _ = self.0.wait();
    }
}

fn set_nonblocking(fd: &impl AsFd) -> Result<(), Errno> {
    let flags = OFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFL)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
enum ConnectionState {
    Waiting,
    Retrying { reason: String, retry_at: Instant },
}

struct Connection {
    state: ConnectionState,
    // Keep errors across reconnects to suppress repeated failures.
    // Clear them only after a requested frame has been saved.
    last_error: Option<String>,
}
impl Connection {
    fn new() -> Self {
        Self {
            state: ConnectionState::Waiting,
            last_error: None,
        }
    }

    fn recovered(&mut self) -> bool {
        self.last_error.take().is_some()
    }

    fn retry(&mut self, reason: String, retry_at: Instant) -> bool {
        let changed = self.last_error.as_ref() != Some(&reason);
        self.last_error = Some(reason.clone());
        self.state = ConnectionState::Retrying { reason, retry_at };
        changed
    }
}

// The collector reserves an idle worker before listing it in a tick manifest.
// No lock is held during process I/O, and a busy worker never queues another tick.
#[derive(Default)]
enum RequestState {
    #[default]
    Disconnected,
    Idle,
    Queued(String),
    Pending(String),
}
#[derive(Default)]
struct Requests(Mutex<RequestState>);
impl Requests {
    fn reserve(&self, tick_id: &str) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, RequestState::Idle) {
            *state = RequestState::Queued(tick_id.into());
            true
        } else {
            false
        }
    }
    fn ready(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = RequestState::Idle;
    }
    fn take(&self) -> Option<String> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let RequestState::Queued(tick_id) = &*state {
            let tick_id = tick_id.clone();
            *state = RequestState::Pending(tick_id.clone());
            Some(tick_id)
        } else {
            None
        }
    }
    fn disconnect(&self) -> Option<String> {
        match std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner())) {
            RequestState::Queued(tick_id) | RequestState::Pending(tick_id) => Some(tick_id),
            _ => None,
        }
    }
}

type SharedConnection = Arc<Mutex<Connection>>;
/// Owns a reporting worker. Dropping it requests a stop and waits for the worker.
/// Call `stop()` on all readers first when shutting down several at once.
pub struct Reader {
    requested: Arc<Requests>,
    stop: Arc<AtomicBool>,
    #[cfg(test)]
    connection: SharedConnection,
    thread: Option<JoinHandle<()>>,
}
impl Reader {
    pub fn spawn(spec: CommandSpec, timing: Timing, target: RawTarget) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let requested = Arc::new(Requests::default());
        let worker_requested = requested.clone();
        let connection = Arc::new(Mutex::new(Connection::new()));
        let (worker_stop, worker_connection) = (stop.clone(), connection.clone());
        let thread = thread::Builder::new()
            .name("guest-metrics".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    worker_connection
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .state = ConnectionState::Waiting;
                    let result = session(
                        &spec,
                        timing,
                        &worker_stop,
                        &worker_connection,
                        &target,
                        &worker_requested,
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
                    let notify = worker_connection
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .retry(reason.clone(), deadline);
                    if notify {
                        eprintln!(
                            "Guest metrics [{}] unavailable: {}; retrying in {:.1}s",
                            spec.source,
                            reason,
                            timing.retry.as_secs_f64()
                        );
                    }
                    while !worker_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                        thread::sleep(POLL_INTERVAL);
                    }
                }
                worker_connection
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .state = ConnectionState::Waiting;
            })?;
        Ok(Self {
            requested,
            stop,
            #[cfg(test)]
            connection,
            thread: Some(thread),
        })
    }
    /// Accept one tick only while connected and idle. Never wait for guest I/O.
    pub fn request(&self, tick_id: &str) -> bool {
        !self.stop.load(Ordering::Relaxed) && self.requested.reserve(tick_id)
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
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

fn session(
    spec: &CommandSpec,
    timing: Timing,
    stop: &AtomicBool,
    connection: &SharedConnection,
    target: &RawTarget,
    requested: &Requests,
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
        while !stop.load(Ordering::Relaxed) {
            captured
                .drain(&mut stderr)
                .map_err(|e| format!("cannot read guest stderr: {e}"))?;
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
                .is_some_and(|(_, sent)| sent.elapsed() > timing.stale)
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
                        let recovered = connection
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .recovered();
                        if recovered {
                            eprintln!("Guest metrics [{}] recovered", spec.source);
                        }
                    }
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted =>
                {
                    thread::sleep(POLL_INTERVAL)
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    })();
    result.map_err(|reason| captured.failure_details(&reason, &proc.0, &mut stderr, stop))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORTER: &str = "import sys\nfor command in sys.stdin:\n assert command == 'sample\\n'\n print('opaque reply',flush=True)";

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
        let (reason, _) = await_retry(&reader);
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
        let (error, _) = await_retry(&reader);
        assert!(error.contains("unsolicited guest report"), "{error}");
        assert!(error.contains("exit status 9"), "{error}");
        assert!(error.contains("stderr: script failed"), "{error}");
        assert_eq!(attempts.frame("test-tick").payload, b"first");
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn stderr_flood_does_not_block_raw_replies() {
        let attempts = Attempts::new();
        let reader = attempts.start("import sys\nsys.stdin.readline()\nsys.stderr.write('x'*2_000_000); sys.stderr.flush()\nprint('opaque reply',flush=True)\ntime.sleep(30)", Timing { stale: Duration::from_secs(2), ..timing() });
        request_when_ready(&reader, "test-tick");
        assert_eq!(attempts.frame("test-tick").payload, b"opaque reply");
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn stderr_only_flood_does_not_refresh_request_timeout() {
        let start = Instant::now();
        let error = session_error(
            "import os; exec(\"while True: os.write(2,b'x'*4096)\")",
            Duration::from_millis(150),
        );
        assert!(error.contains("reporter timed out"), "{error}");
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

    #[test]
    fn connection_suppresses_repeat_errors_until_a_frame_recovers() {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut connection = Connection::new();
        assert_eq!(connection.state, ConnectionState::Waiting);
        assert!(!connection.recovered());
        assert!(connection.retry("reporter timed out".into(), deadline));
        assert_eq!(
            connection.state,
            ConnectionState::Retrying {
                reason: "reporter timed out".into(),
                retry_at: deadline
            }
        );
        // Opening a new connection alone does not mean it can deliver replies.
        connection.state = ConnectionState::Waiting;
        assert!(!connection.retry("reporter timed out".into(), deadline));
        assert!(connection.retry("guest stream closed".into(), deadline));
        assert!(connection.recovered());
        assert_eq!(connection.last_error, None);
        assert!(!connection.recovered());
        assert!(connection.retry("guest stream closed".into(), deadline));
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
                identity: "23/test".into(),
            }
        }
        fn start(&self, body: &str, timing: Timing) -> Reader {
            Reader::spawn(spec(&self.script(body)), timing, self.target()).unwrap()
        }
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

    fn await_retry(reader: &Reader) -> (String, Instant) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let ConnectionState::Retrying { reason, retry_at } =
                &reader.connection.lock().unwrap().state
            {
                return (reason.clone(), *retry_at);
            }
            assert!(
                Instant::now() < deadline,
                "reader did not enter retry state"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

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
        assert_eq!(first.identity, "23/test");
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
    fn unexpected_eof_records_retry_reason_and_deadline() {
        let attempts = Attempts::new();
        let reader = attempts.start(
            "pass",
            Timing {
                retry: Duration::from_secs(30),
                ..timing()
            },
        );
        let (reason, retry_at) = await_retry(&reader);
        assert!(reason.starts_with("guest stream closed"), "{reason}");
        assert!(reason.contains("exit status 0"), "{reason}");
        assert!(retry_at > Instant::now());
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
        let (reason, _) = await_retry(&reader);
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
                let (reason, _) = await_retry(&reader);
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
    fn request_slots_keep_tick_ids_and_skip_busy_connections() {
        let requests = Requests::default();
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
                identity: "23/test".into(),
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
        assert!(!matches!(
            reader.connection.lock().unwrap().state,
            ConnectionState::Retrying { .. }
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
                identity: "23/test".into(),
            },
        )
        .unwrap();
        request_when_ready(&reader, "test-tick");
        await_retry(&reader);
        assert!(
            matches!(&reader.connection.lock().unwrap().state, ConnectionState::Retrying { reason, .. } if reason.starts_with("reporter timed out"))
        );
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
            &format!("if len(p.read_text().splitlines()) <= 3: raise SystemExit(7)\n{REPORTER}"),
            Timing {
                stale: Duration::from_secs(30),
                retry: Duration::from_millis(200),
            },
        );
        await_retry(&reader);
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
        // The raw file is renamed just before recovery is recorded.
        let deadline = Instant::now() + Duration::from_secs(1);
        while reader.connection.lock().unwrap().last_error.is_some() {
            assert!(Instant::now() < deadline, "recovery was not recorded");
            thread::sleep(Duration::from_millis(5));
        }
        let start = Instant::now();
        drop(reader);
        assert!(start.elapsed() < Duration::from_secs(1));
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
        await_retry(&reader);
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
        await_retry(&reader);
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
        await_retry(&reader);
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
        let cmd = CommandSpec::qube("dev", Kind::Memory(23));
        assert_eq!(cmd.program, "qvm-run");
        assert!(cmd.args.iter().any(|s| s == "--no-autostart"));
        assert!(cmd.args.iter().any(|s| s == "--no-gui"));
        assert!(cmd.args.last().unwrap().contains("xenstore-read"));
        assert_eq!(cmd.source, "dev RAM");
        assert_eq!(
            CommandSpec::qube("sys-net", Kind::Network).source,
            "sys-net network"
        );
    }
}
