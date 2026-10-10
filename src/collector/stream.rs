use super::protocol::{shell_quote, Kind, Reading, Tracker, MEMORY_SCRIPT, NETWORK_SCRIPT};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
// Guest scripts then exit on their next write to the closed stdout connection.
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
pub struct Reader {
    stop: Arc<AtomicBool>,
    connection: SharedConnection,
    thread: Option<JoinHandle<()>>,
    timing: Timing,
}
impl Reader {
    pub fn spawn(spec: CommandSpec, kind: Kind, timing: Timing) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
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
                    let result = session(&spec, kind, timing, &worker_stop, &worker_connection);
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
            stop,
            connection,
            thread: Some(thread),
            timing,
        })
    }
    pub fn value(&self) -> Option<Reading> {
        if self.stop.load(Ordering::Relaxed) {
            return None;
        }
        self.connection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .value(Instant::now(), self.timing.stale)
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
    kind: Kind,
    timing: Timing,
    stop: &AtomicBool,
    connection: &SharedConnection,
) -> Result<(), String> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut proc = Process(command.spawn().map_err(|e| e.to_string())?);
    let mut stdout = proc.0.stdout.take().ok_or("missing stdout")?;
    set_nonblocking(&stdout).map_err(|e| e.to_string())?;
    let mut tracker = Tracker::new(kind);
    let mut buffer = Vec::new();
    let mut chunk = [0; 4096];
    let mut last = Instant::now();
    let mut budget_time = last;
    let mut budget = 8.0_f64;
    while !stop.load(Ordering::Relaxed) {
        if last.elapsed() > timing.stale {
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
                    let reading = tracker.update(&buffer[..end])?;
                    buffer.drain(..=end);
                    last = now;
                    let recovered = connection
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .report(reading, now);
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(connection.report(Some(Reading::Memory(0)), now));
        assert_eq!(connection.last_error, None);
        assert_eq!(
            connection.value(now, Duration::from_secs(5)),
            Some(Reading::Memory(0))
        );
        assert!(!connection.report(Some(Reading::Memory(100)), now));
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
        await_value(&reader, Reading::Memory(600));
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
                assert_eq!(reason, "guest stream closed");
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
        await_value(&reader, Reading::Memory(600));
        // Observe connection timeout/retry (then another good sample).
        let deadline = Instant::now() + Duration::from_secs(3);
        while reader.value().is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(reader.value().is_none());
        await_value(&reader, Reading::Memory(600));
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
    #[test]
    fn qubes_command_never_autostarts() {
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
