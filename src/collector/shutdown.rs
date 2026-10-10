use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::{flag, low_level, SigId};
use std::io;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// Own signal registrations for one collector run; workers start after registration.
pub struct Shutdown {
    requested: Arc<AtomicBool>,
    handlers: Vec<SigId>,
}

impl Shutdown {
    pub fn register() -> io::Result<Self> {
        let mut shutdown = Self {
            requested: Arc::new(AtomicBool::new(false)),
            handlers: Vec::new(),
        };
        for signal in [SIGINT, SIGTERM] {
            shutdown
                .handlers
                .push(flag::register(signal, shutdown.requested.clone())?);
        }
        Ok(shutdown)
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
        let deadline = Instant::now() + Duration::from_secs(5);
        while !shutdown.requested() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(shutdown.requested());
    }

    #[test]
    fn handles_int_and_term() {
        // Never change the test runner's process-wide signal disposition.
        for signal in [Signal::SIGINT, Signal::SIGTERM] {
            let path =
                std::env::temp_dir().join(format!("mq-signal-{}-{signal}", std::process::id()));
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "collector::shutdown::tests::signal_child",
                    "--ignored",
                ])
                .env("MQ_SIGNAL_READY", &path)
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
