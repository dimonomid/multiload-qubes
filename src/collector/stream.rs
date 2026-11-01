//! Keeps one guest reporting connection open and retries when it fails.
//!
//! Each reader owns a worker thread and a local qvm-run process group. The
//! sampling loop only reads the latest value; it does not wait for guest I/O.
use super::diagnostics::StderrCapture;
use crate::metrics::handoff::{RawFrame, MAX_REPORT_BYTES};
use crate::metrics::protocol::{Kind, Reading, Tracker};
use crate::metrics::publish_bytes;
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::signal::{killpg, Signal};
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

pub const MEMORY_SCRIPT: &str = include_str!("../../guest/memory.py");
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
    pub fn publish(&self, session: &str, payload: &[u8]) -> Result<(), String> {
        if payload.len() > MAX_REPORT_BYTES {
            return Err("oversized raw report".into());
        }
        let frame = RawFrame {
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
            Kind::Memory(_) => MEMORY_SCRIPT,
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
            stale: Duration::from_secs(5),
            retry: Duration::from_secs(3),
        }
    }
}

// Dropping on any error/stop terminates the entire local qvm-run process group.
// Guest scripts exit when their stdin closes (or a write sees a broken pipe).
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
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.0.try_wait() {
                Ok(Some(_)) => return,
                _ if Instant::now() >= deadline => break,
                _ => thread::sleep(Duration::from_millis(20)),
            }
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
    Reporting {
        reading: Reading,
        received_at: Instant,
    },
    Retrying {
        reason: String,
        retry_at: Instant,
    },
}

struct Connection {
    state: ConnectionState,
    // Retained across reconnect/warmup attempts to suppress repeated failures.
    // Cleared only when an actual measurement recovers.
    last_error: Option<String>,
}
impl Connection {
    fn new() -> Self {
        Self {
            state: ConnectionState::Waiting,
            last_error: None,
        }
    }

    fn report(&mut self, reading: Option<Reading>, received_at: Instant) -> bool {
        match reading {
            Some(reading) => {
                self.state = ConnectionState::Reporting {
                    reading,
                    received_at,
                };
                self.last_error.take().is_some()
            }
            None => {
                self.state = ConnectionState::Waiting;
                false
            }
        }
    }

    fn retry(&mut self, reason: String, retry_at: Instant) -> bool {
        let changed = self.last_error.as_ref() != Some(&reason);
        self.last_error = Some(reason.clone());
        self.state = ConnectionState::Retrying { reason, retry_at };
        changed
    }

    #[cfg(test)]
    fn value(&self, now: Instant, stale: Duration) -> Option<Reading> {
        match self.state {
            ConnectionState::Reporting {
                reading,
                received_at,
            } if now.saturating_duration_since(received_at) <= stale => Some(reading),
            _ => None,
        }
    }
}

type SharedConnection = Arc<Mutex<Connection>>;
/// Owns a reporting worker. Dropping it requests a stop and waits for the worker.
/// Call `stop()` on all readers first when shutting down several at once.
pub struct Reader {
    requested: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    #[cfg(test)]
    connection: SharedConnection,
    thread: Option<JoinHandle<()>>,
    #[cfg(test)]
    timing: Timing,
}
impl Reader {
    #[cfg(test)]
    pub fn spawn(spec: CommandSpec, kind: Kind, timing: Timing) -> io::Result<Self> {
        Self::spawn_inner(spec, kind, timing, None)
    }
    pub fn spawn_raw(
        spec: CommandSpec,
        kind: Kind,
        timing: Timing,
        target: RawTarget,
    ) -> io::Result<Self> {
        Self::spawn_inner(spec, kind, timing, Some(target))
    }
    fn spawn_inner(
        spec: CommandSpec,
        kind: Kind,
        timing: Timing,
        target: Option<RawTarget>,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let requested = Arc::new(AtomicBool::new(false));
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
                    let result = session_inner(
                        &spec,
                        kind,
                        timing,
                        &worker_stop,
                        &worker_connection,
                        target.as_ref(),
                        &worker_requested,
                    );
                    if let Some(target) = &target {
                        // An empty frame clears the parser's previous value on
                        // disconnect instead of leaving it visible until timeout.
                        let _ = target.publish("disconnected", &[]);
                    }
                    if worker_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let reason = result.err().unwrap_or_else(|| "guest stream closed".into());
                    let (notify, state) = {
                        let mut connection =
                            worker_connection.lock().unwrap_or_else(|e| e.into_inner());
                        let notify = connection.retry(reason, Instant::now() + timing.retry);
                        (notify, connection.state.clone())
                    };
                    let ConnectionState::Retrying {
                        reason,
                        retry_at: deadline,
                    } = state
                    else {
                        unreachable!("retry records Retrying state")
                    };
                    if notify {
                        eprintln!(
                            "Guest metrics [{}] unavailable: {}; retrying in {:.1}s",
                            spec.source,
                            reason,
                            timing.retry.as_secs_f64()
                        );
                    }
                    while !worker_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(20));
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
            #[cfg(test)]
            timing,
        })
    }
    /// Returns no value while connecting, after a failure, or once a report is
    /// too old. A stopped reader never returns its last measurement.
    #[cfg(test)]
    pub fn value(&self) -> Option<Reading> {
        if self.stop.load(Ordering::Relaxed) {
            return None;
        }
        self.connection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .value(Instant::now(), self.timing.stale)
    }
    /// Coalesce ticks without blocking the host sampling loop.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Relaxed);
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

#[cfg(test)]
fn session(
    spec: &CommandSpec,
    kind: Kind,
    timing: Timing,
    stop: &AtomicBool,
    connection: &SharedConnection,
) -> Result<(), String> {
    session_inner(
        spec,
        kind,
        timing,
        stop,
        connection,
        None,
        &AtomicBool::new(false),
    )
}

fn session_inner(
    spec: &CommandSpec,
    kind: Kind,
    timing: Timing,
    stop: &AtomicBool,
    connection: &SharedConnection,
    target: Option<&RawTarget>,
    requested: &AtomicBool,
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
    let result = (|| -> Result<(), String> {
        let mut tracker = Tracker::new(kind);
        let mut buffer = Vec::new();
        let mut chunk = [0; 4096];
        let mut last = Instant::now();
        let mut pending: Option<Instant> = None;
        let mut budget_time = last;
        // Allow a short burst of buffered lines, but reject a stream that keeps
        // sending more than four samples per second. Guest output is untrusted.
        let mut budget = 8.0_f64;
        while !stop.load(Ordering::Relaxed) {
            captured
                .drain(&mut stderr)
                .map_err(|e| format!("cannot read guest stderr: {e}"))?;
            if target.is_some() {
                let tick = requested.swap(false, Ordering::Relaxed);
                // Ignore ticks while a reply is outstanding; never queue catch-up
                // requests for a slow guest.
                if pending.is_none() && tick {
                    let request = b"sample\n";
                    match stdin.write(request) {
                        Ok(n) if n == request.len() => pending = Some(Instant::now()),
                        Ok(_) => return Err("incomplete sample request".into()),
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) => {}
                        Err(error) => return Err(format!("cannot request guest sample: {error}")),
                    }
                }
            }
            let timed_out = if target.is_some() {
                pending.is_some_and(|sent| sent.elapsed() > timing.stale)
            } else {
                last.elapsed() > timing.stale
            };
            if timed_out {
                return Err("reporter timed out".into());
            }
            match stdout.read(&mut chunk) {
                Ok(0) => return Err("guest stream closed".into()),
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    if buffer.len() > 8192 {
                        return Err("oversized guest stream".into());
                    }
                    while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                        let now = Instant::now();
                        budget = (budget + now.duration_since(budget_time).as_secs_f64() * 4.0)
                            .min(8.0)
                            - 1.0;
                        budget_time = now;
                        if budget < 0.0 {
                            return Err("guest exceeded sample rate limit".into());
                        }
                        let reading = if let Some(target) = target {
                            if pending.take().is_none() {
                                return Err("unsolicited guest report".into());
                            }
                            target.publish(&session, &buffer[..end])?;
                            None
                        } else {
                            tracker.update(&buffer[..end])?
                        };
                        buffer.drain(..=end);
                        // Transport liveness depends on receiving bounded frames;
                        // only the restricted parser decides if metrics are valid.
                        last = now;
                        let recovered = {
                            let mut connection =
                                connection.lock().unwrap_or_else(|e| e.into_inner());
                            if target.is_some() {
                                connection.last_error.take().is_some()
                            } else {
                                connection.report(reading, now)
                            }
                        };
                        if recovered {
                            eprintln!("Guest metrics [{}] recovered", spec.source);
                        }
                    }
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted =>
                {
                    thread::sleep(Duration::from_millis(20))
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

    fn session_error(script: &str, stale: Duration) -> String {
        session(
            &spec(script),
            Kind::Memory(23),
            Timing { stale, ..timing() },
            &AtomicBool::new(false),
            &Arc::new(Mutex::new(Connection::new())),
        )
        .unwrap_err()
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
        let error = session_error("import sys; sys.stderr.write('script failed\\n'); sys.stderr.flush(); print('M1 24 1000 400',flush=True); sys.exit(9)", Duration::from_secs(2));
        assert!(error.contains("changed domain"), "{error}");
        assert!(error.contains("exit status 9"), "{error}");
        assert!(error.contains("stderr: script failed"), "{error}");
    }

    #[test]
    fn stderr_flood_does_not_block_usable_metrics() {
        let reader = Reader::spawn(spec("import sys,time; sys.stderr.write('x'*2_000_000); sys.stderr.flush(); print('M1 23 1000 400',flush=True); time.sleep(20)"),
            Kind::Memory(23), Timing { stale: Duration::from_secs(2), ..timing() }).unwrap();
        await_value(
            &reader,
            Reading::Memory {
                used: 600,
                total: 1000,
                swap: None,
            },
        );
    }

    #[test]
    fn stderr_only_flood_does_not_refresh_metrics_timeout() {
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
        let error = session_error("import os,sys,time; sys.stderr.write('stdout closed\\n'); sys.stderr.flush(); os.close(1); time.sleep(20)", Duration::from_secs(2));
        assert!(error.contains("guest stream closed"), "{error}");
        assert!(error.contains("exit status not yet available"), "{error}");
        assert!(error.contains("stderr: stdout closed"), "{error}");
        // Do not report our cleanup SIGTERM as the cause of the failure.
        assert!(!error.contains("SIGTERM"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn connection_states_suppress_repeat_errors_and_log_one_recovery() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(3);
        let mut connection = Connection::new();
        assert_eq!(connection.state, ConnectionState::Waiting);
        assert_eq!(connection.value(now, Duration::from_secs(5)), None);
        assert!(connection.retry("reporter timed out".into(), deadline));
        assert_eq!(
            connection.state,
            ConnectionState::Retrying {
                reason: "reporter timed out".into(),
                retry_at: deadline,
            }
        );
        // A reconnect/warmup does not erase the last failure or claim recovery.
        connection.state = ConnectionState::Waiting;
        assert!(!connection.report(None, now));
        assert!(!connection.retry("reporter timed out".into(), deadline));
        assert!(connection.retry("guest stream closed".into(), deadline));
        assert!(connection.report(
            Some(Reading::Memory {
                used: 0,
                total: 1000,
                swap: None
            }),
            now
        ));
        assert_eq!(connection.last_error, None);
        assert_eq!(
            connection.value(now, Duration::from_secs(5)),
            Some(Reading::Memory {
                used: 0,
                total: 1000,
                swap: None
            })
        );
        assert!(!connection.report(
            Some(Reading::Memory {
                used: 100,
                total: 1000,
                swap: None
            }),
            now
        ));
        // A new failure after a real recovery deserves a new diagnostic.
        assert!(connection.retry("guest stream closed".into(), deadline));
    }

    #[test]
    fn stale_data_is_derived_and_warmup_has_no_measurement() {
        let now = Instant::now();
        let stale = Duration::from_secs(5);
        let mut connection = Connection::new();
        assert!(!connection.report(Some(Reading::Network(0.0, 0.0)), now));
        assert_eq!(
            connection.value(now + stale, stale),
            Some(Reading::Network(0.0, 0.0))
        );
        assert_eq!(
            connection.value(now + stale + Duration::from_nanos(1), stale),
            None
        );
        assert!(!connection.report(None, now + stale));
        assert_eq!(connection.state, ConnectionState::Waiting);
        assert_eq!(connection.value(now + stale, stale), None);
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
    fn await_value(reader: &Reader, expected: Reading) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while reader.value() != Some(expected) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(reader.value(), Some(expected));
    }
    #[test]
    fn persistent_memory_and_stop_cleanup() {
        let reader = Reader::spawn(
            spec("import time; print('M1 23 1000 400',flush=True); time.sleep(20)"),
            Kind::Memory(23),
            timing(),
        )
        .unwrap();
        await_value(
            &reader,
            Reading::Memory {
                used: 600,
                total: 1000,
                swap: None,
            },
        );
        assert!(matches!(
            reader.connection.lock().unwrap().state,
            ConnectionState::Reporting { .. }
        ));
        let start = Instant::now();
        reader.stop();
        assert_eq!(reader.value(), None);
        drop(reader);
        assert!(start.elapsed() < Duration::from_secs(3));
    }
    #[test]
    fn unexpected_eof_records_retry_reason_and_deadline() {
        let reader = Reader::spawn(
            spec("pass"),
            Kind::Memory(23),
            Timing {
                retry: Duration::from_secs(2),
                ..timing()
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let state = reader.connection.lock().unwrap().state.clone();
            if let ConnectionState::Retrying { reason, retry_at } = state {
                assert!(reason.starts_with("guest stream closed"));
                assert!(reason.contains("exit status 0"));
                assert!(retry_at > Instant::now());
                assert_eq!(reader.value(), None);
                break;
            }
            assert!(Instant::now() < deadline, "EOF did not enter retry state");
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn stale_readings_and_reconnection() {
        let reader = Reader::spawn(
            spec("import time; print('M1 23 1000 400',flush=True); time.sleep(20)"),
            Kind::Memory(23),
            timing(),
        )
        .unwrap();
        await_value(
            &reader,
            Reading::Memory {
                used: 600,
                total: 1000,
                swap: None,
            },
        );
        // Observe connection timeout/retry (then another good sample).
        let deadline = Instant::now() + Duration::from_secs(3);
        while reader.value().is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(reader.value().is_none());
        await_value(
            &reader,
            Reading::Memory {
                used: 600,
                total: 1000,
                swap: None,
            },
        );
    }
    #[test]
    fn rejects_wrong_domain_flood_and_oversized_input() {
        for script in [
            "import time; print('M1 24 1000 400',flush=True); time.sleep(20)",
            "print('x'*10000,flush=True)",
            "print('M1 23 1000 400\\n'*100,flush=True)",
        ] {
            let reader = Reader::spawn(
                spec(script),
                Kind::Memory(23),
                Timing {
                    retry: Duration::from_secs(2),
                    ..timing()
                },
            )
            .unwrap();
            thread::sleep(Duration::from_millis(150));
            assert!(reader.value().is_none());
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

    fn await_retry(reader: &Reader) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if matches!(
                reader.connection.lock().unwrap().state,
                ConnectionState::Retrying { .. }
            ) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "reader did not enter retry state"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn requests_are_tick_driven_and_do_not_queue_behind_a_slow_reply() {
        let attempts = Attempts::new();
        let requests = attempts.0.join("requests");
        let gate = attempts.0.join("reply");
        let target = attempts.0.join("raw");
        let body = format!("import sys\nrequests=pathlib.Path({requests:?})\ngate=pathlib.Path({gate:?})\nfor command in sys.stdin:\n with requests.open('a') as f: f.write(command)\n while not gate.exists(): time.sleep(.005)\n print('M1 23 1000 400',flush=True)");
        let reader = Reader::spawn_raw(
            spec(&attempts.script(&body)),
            Kind::Memory(23),
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
        reader.request();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !requests.exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..10 {
            reader.request();
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
        thread::sleep(Duration::from_millis(40));
        assert_eq!(std::fs::read_to_string(&requests).unwrap(), "sample\n");
        reader.request();
        let deadline = Instant::now() + Duration::from_secs(3);
        while std::fs::read_to_string(&requests).unwrap().lines().count() < 2 {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn unanswered_requests_time_out_and_are_reaped() {
        let attempts = Attempts::new();
        let reader = Reader::spawn_raw(
            spec(&attempts.script("import sys\nsys.stdin.readline()\ntime.sleep(30)")),
            Kind::Memory(23),
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
        reader.request();
        await_retry(&reader);
        assert!(
            matches!(&reader.connection.lock().unwrap().state, ConnectionState::Retrying { reason, .. } if reason.starts_with("reporter timed out"))
        );
        drop(reader);
        attempts.assert_reaped();
    }

    #[test]
    fn repeated_failed_reconnects_are_reaped_and_eventually_recover() {
        let attempts = Attempts::new();
        let reader = Reader::spawn(
            spec(&attempts.script(
                "if len(p.read_text().splitlines()) <= 3: raise SystemExit(7)\nprint('M1 23 1000 400',flush=True)\ntime.sleep(30)",
            )),
            Kind::Memory(23),
            Timing {
                stale: Duration::from_secs(30),
                retry: Duration::from_millis(200),
            },
        )
        .unwrap();
        await_retry(&reader);
        assert_eq!(reader.value(), None);
        attempts.assert_reaped();
        // A failed attempt must wait for its retry deadline, not spin spawning.
        let count = attempts.pids().len();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(attempts.pids().len(), count);
        await_value(
            &reader,
            Reading::Memory {
                used: 600,
                total: 1000,
                swap: None,
            },
        );
        assert_eq!(attempts.pids().len(), 4);
        assert_eq!(reader.connection.lock().unwrap().last_error, None);
        let start = Instant::now();
        drop(reader);
        assert!(start.elapsed() < Duration::from_secs(1));
        attempts.assert_reaped();
    }

    #[test]
    fn failed_spawn_can_recover_and_stop_during_retry() {
        let attempts = Attempts::new();
        let program = attempts.0.join("reporter");
        let mut command =
            spec(&attempts.script("print('M1 23 1000 400',flush=True)\ntime.sleep(30)"));
        command.program = program.to_str().unwrap().into();
        let reader = Reader::spawn(
            command.clone(),
            Kind::Memory(23),
            Timing {
                stale: Duration::from_secs(30),
                retry: Duration::from_millis(100),
            },
        )
        .unwrap();
        await_retry(&reader);
        assert_eq!(reader.value(), None);
        assert!(attempts.pids().is_empty());
        std::os::unix::fs::symlink("/usr/bin/python3", &program).unwrap();
        await_value(
            &reader,
            Reading::Memory {
                used: 600,
                total: 1000,
                swap: None,
            },
        );
        drop(reader);
        attempts.assert_reaped();

        std::fs::remove_file(program).unwrap();
        let reader = Reader::spawn(
            command,
            Kind::Memory(23),
            Timing {
                retry: Duration::from_secs(30),
                ..timing()
            },
        )
        .unwrap();
        await_retry(&reader);
        let start = Instant::now();
        drop(reader);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(attempts.pids().len(), 1);
    }

    #[test]
    fn shutdown_interrupts_silent_and_reporting_sessions_and_reaps_children() {
        for body in [
            "time.sleep(30)",
            "print('M1 23 1000 400',flush=True)\ntime.sleep(30)",
        ] {
            let attempts = Attempts::new();
            let reader = Reader::spawn(
                spec(&attempts.script(body)),
                Kind::Memory(23),
                Timing {
                    stale: Duration::from_secs(30),
                    retry: Duration::from_secs(30),
                },
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while attempts.pids().is_empty() {
                assert!(Instant::now() < deadline, "helper never started");
                thread::sleep(Duration::from_millis(5));
            }
            if body.starts_with("print") {
                await_value(
                    &reader,
                    Reading::Memory {
                        used: 600,
                        total: 1000,
                        swap: None,
                    },
                );
            }
            let start = Instant::now();
            reader.stop();
            assert_eq!(reader.value(), None);
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
        let reader = Reader::spawn(
            spec(&attempts.script("raise SystemExit(7)")),
            Kind::Memory(23),
            Timing {
                retry: Duration::from_secs(30),
                ..timing()
            },
        )
        .unwrap();
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
