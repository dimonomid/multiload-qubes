//! Signal handlers set a flag and wake the main loop. Worker shutdown and
//! process cleanup happen outside the signal handler.
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::pipe2;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::{flag, low_level, SigId};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::{
    io,
    os::fd::{AsFd, OwnedFd},
    time::Instant,
};

/// Own signal registrations and a wakeup pipe for one service run.
pub struct Shutdown {
    /// Set by signal handlers and read by the main loop.
    requested: Arc<AtomicBool>,
    /// Registrations removed on drop, including after a partial registration failure.
    handlers: Vec<SigId>,
    /// Read end of the signal wakeup pipe; registered handlers own the write ends.
    wake: OwnedFd,
}

impl Shutdown {
    /// Register both shutdown signals before starting workers.
    pub fn register() -> io::Result<Self> {
        // A plain pipe also works in the parser sandbox, which forbids sockets.
        let (wake, write) = pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)?;
        let mut shutdown = Self {
            requested: Arc::new(AtomicBool::new(false)),
            handlers: Vec::new(),
            wake,
        };
        for signal in [SIGINT, SIGTERM] {
            shutdown
                .handlers
                .push(flag::register(signal, shutdown.requested.clone())?);
            // Register in this order so the flag is set before poll wakes.
            shutdown
                .handlers
                .push(low_level::pipe::register(signal, write.try_clone()?)?);
        }
        Ok(shutdown)
    }

    /// Sleep until the sampling deadline or shutdown, without periodic flag checks.
    pub fn wait_until(&self, deadline: Instant) -> io::Result<()> {
        let mut fds = [PollFd::new(self.wake.as_fd(), PollFlags::POLLIN)];
        while !self.requested() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            // poll takes integer milliseconds. Round up so the last fraction
            // does not cause a busy loop, and recompute after interrupted waits.
            let millis = (deadline - now)
                .as_nanos()
                .div_ceil(1_000_000)
                .min(i32::MAX as u128) as i32;
            match poll(&mut fds, PollTimeout::try_from(millis).unwrap()) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
        // The flag never resets, so the notification need not be drained. A
        // signal before poll stays readable; later calls return from the flag.
        Ok(())
    }

    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }
}

impl Drop for Shutdown {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            low_level::unregister(handler);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    use std::{
        fs,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    #[test]
    #[ignore = "invoked in a child process by handles_int_and_term"]
    fn signal_child() {
        let path = std::env::var_os("MQ_SIGNAL_READY").unwrap();
        let shutdown = Shutdown::register().unwrap();
        assert!(!shutdown.requested());
        fs::write(path, "ready").unwrap();
        if std::env::var_os("MQ_SIGNAL_DEADLINE").is_some() {
            let start = Instant::now();
            shutdown.wait_until(start).unwrap();
            assert!(start.elapsed() < Duration::from_secs(1));
            shutdown
                .wait_until(start + Duration::from_millis(80))
                .unwrap();
            assert!(start.elapsed() >= Duration::from_millis(80));
            assert!(start.elapsed() < Duration::from_secs(2));
            assert!(!shutdown.requested());
            return;
        }
        if std::env::var_os("MQ_SIGNAL_BEFORE_WAIT").is_some() {
            while !shutdown.requested() {
                thread::sleep(Duration::from_millis(1));
            }
        }
        shutdown
            .wait_until(Instant::now() + Duration::from_secs(30))
            .unwrap();
        assert!(shutdown.requested());
        // An already requested shutdown must not block on a second wait either.
        shutdown
            .wait_until(Instant::now() + Duration::from_secs(30))
            .unwrap();
    }

    #[test]
    fn wait_returns_at_deadline_or_immediately_when_already_past_it() {
        let path = std::env::temp_dir().join(format!("mq-signal-deadline-{}", std::process::id()));
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "metrics::shutdown::tests::signal_child",
                "--ignored",
            ])
            .env("MQ_SIGNAL_READY", &path)
            .env("MQ_SIGNAL_DEADLINE", "1")
            .status()
            .unwrap();
        let _ = fs::remove_file(path);
        assert!(status.success());
    }

    #[test]
    fn handles_int_and_term() {
        // Never change the test runner's process-wide signal disposition.
        for (signal, before_wait) in [
            (Signal::SIGINT, false),
            (Signal::SIGTERM, false),
            (Signal::SIGTERM, true),
        ] {
            let path =
                std::env::temp_dir().join(format!("mq-signal-{}-{signal}", std::process::id()));
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "metrics::shutdown::tests::signal_child",
                    "--ignored",
                ])
                .env("MQ_SIGNAL_READY", &path)
                .envs(before_wait.then_some(("MQ_SIGNAL_BEFORE_WAIT", "1")))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !path.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if !path.exists() {
                let _ = child.kill();
                let _ = child.wait();
                panic!("signal helper did not become ready");
            }
            fs::remove_file(&path).unwrap();
            kill(Pid::from_raw(child.id() as i32), signal).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("signal helper did not exit");
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
