use super::protocol::{shell_quote, Kind, Reading, Tracker, MEMORY_SCRIPT, NETWORK_SCRIPT};
use std::ffi::c_int;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

extern "C" {
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    fn kill(pid: c_int, signal: c_int) -> c_int;
}

#[derive(Clone)]
pub struct CommandSpec {
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
impl Drop for Process {
    fn drop(&mut self) {
        let pid = self.0.id() as i32;
        unsafe {
            kill(-pid, 15);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.0.try_wait() {
                Ok(Some(_)) => return,
                _ if Instant::now() >= deadline => break,
                _ => thread::sleep(Duration::from_millis(20)),
            }
        }
        unsafe {
            kill(-pid, 9);
        }
        let _ = self.0.wait();
    }
}

type Latest = Arc<Mutex<Option<(Instant, Option<Reading>)>>>;
pub struct Reader {
    stop: Arc<AtomicBool>,
    latest: Latest,
    thread: Option<JoinHandle<()>>,
    timing: Timing,
}
impl Reader {
    pub fn spawn(spec: CommandSpec, kind: Kind, timing: Timing) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let latest: Latest = Arc::new(Mutex::new(None));
        let (worker_stop, worker_latest) = (stop.clone(), latest.clone());
        let thread = thread::Builder::new()
            .name("guest-metrics".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    let _ = session(&spec, kind, timing, &worker_stop, &worker_latest);
                    *worker_latest.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    let deadline = Instant::now() + timing.retry;
                    while !worker_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(20));
                    }
                }
            })?;
        Ok(Self {
            stop,
            latest,
            thread: Some(thread),
            timing,
        })
    }
    pub fn value(&self) -> Option<Reading> {
        let value = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        value
            .as_ref()
            .filter(|(time, _)| time.elapsed() <= self.timing.stale)
            .and_then(|(_, v)| *v)
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
    latest: &Latest,
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
    // Linux F_GETFL/F_SETFL and O_NONBLOCK. A fixed buffer bounds untrusted input.
    let flags = unsafe { fcntl(stdout.as_raw_fd(), 3) };
    if flags < 0 || unsafe { fcntl(stdout.as_raw_fd(), 4, flags | 0x800) } < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
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
            Ok(0) => return Ok(()),
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
                    *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some((now, reading));
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
    fn spec(script: &str) -> CommandSpec {
        CommandSpec {
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
        let start = Instant::now();
        drop(reader);
        assert!(start.elapsed() < Duration::from_secs(3));
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
    }
}
