//! Bounded, nonblocking capture of untrusted command diagnostics.
use nix::sys::wait::{waitid, Id, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::io::{self, Read};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// Security limit: a compromised qube could keep its dom0 worker busy by flooding
// stderr, even while answering requests. Reporters normally write no stderr;
// exceeding this lifetime byte limit disconnects the connection and triggers backoff.
const STDERR_LIMIT: usize = 64 * 1024;
// Keep only this many recent stderr bytes; older bytes are discarded.
const CAPTURE_LIMIT: usize = 4096;
// Maximum sanitized stderr excerpt included in a failure message, in bytes.
const LOG_LIMIT: usize = 1024;
// Briefly wait for exit status when reporting a failure; process cleanup has its own deadline.
const EXIT_GRACE: Duration = Duration::from_millis(250);

/// Keep a bounded stderr tail for failures without letting a full pipe block the guest.
#[derive(Default)]
pub(super) struct StderrCapture {
    /// Newest stderr bytes, still untrusted and not yet safe for the journal.
    tail: Vec<u8>,
    /// Whether any older bytes were discarded.
    truncated: bool,
    /// All stderr bytes read during this connection, including discarded bytes.
    received: usize,
}

impl StderrCapture {
    fn append(&mut self, bytes: &[u8]) {
        self.truncated |= self.tail.len() + bytes.len() > CAPTURE_LIMIT;
        if bytes.len() >= CAPTURE_LIMIT {
            self.tail.clear();
            self.tail
                .extend_from_slice(&bytes[bytes.len() - CAPTURE_LIMIT..]);
        } else {
            let excess = (self.tail.len() + bytes.len()).saturating_sub(CAPTURE_LIMIT);
            self.tail.drain(..excess);
            self.tail.extend_from_slice(bytes);
        }
    }

    /// Drain a nonblocking pipe, retaining its tail and enforcing the lifetime byte limit.
    /// Return false at EOF so the worker stops watching the closed descriptor.
    pub fn drain(&mut self, pipe: &mut impl Read) -> io::Result<bool> {
        let mut bytes = [0; 4096];
        while self.received <= STDERR_LIMIT {
            match pipe.read(&mut bytes) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.received += n;
                    self.append(&bytes[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        // Later diagnostic reads must stop too, rather than draining more of a flood.
        Err(io::Error::other(
            "guest exceeded stderr limit (64 KiB per connection)",
        ))
    }

    /// Strip terminal escapes and controls, then bound the ASCII text sent to the journal.
    fn excerpt(&self) -> Option<String> {
        let stripped = strip_ansi_escapes::strip(&self.tail);
        let mut text = String::new();
        let mut space = false;
        for ch in String::from_utf8_lossy(&stripped).chars() {
            if ch.is_whitespace() {
                space = !text.is_empty();
                continue;
            }
            if ch.is_control() {
                continue;
            }
            if space {
                text.push(' ');
                space = false;
            }
            if ch.is_ascii() {
                text.push(ch);
            } else {
                text.extend(ch.escape_unicode());
            }
        }
        if text.is_empty() {
            return None;
        }
        let truncated = self.truncated || text.len() > LOG_LIMIT;
        // Everything is ASCII now, so cutting the tail cannot split UTF-8.
        if text.len() > LOG_LIMIT {
            text.drain(..text.len() - LOG_LIMIT);
        }
        Some(if truncated {
            format!("[truncated] {text}")
        } else {
            text
        })
    }

    /// Add bounded stderr and a briefly awaited exit status without reaping the owned process.
    pub fn failure_details(
        &mut self,
        reason: &str,
        child: &Child,
        stderr: &mut impl Read,
        stop: &AtomicBool,
    ) -> String {
        let deadline = Instant::now() + EXIT_GRACE;
        let status = loop {
            let _ = self.drain(stderr);
            // Peek without reaping: Process still owns the PID/process group
            // until its cleanup signals the group and reaps the direct child.
            match waitid(
                Id::Pid(Pid::from_raw(child.id() as i32)),
                WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
            ) {
                Ok(WaitStatus::Exited(_, code)) => break format!("exit status {code}"),
                Ok(WaitStatus::Signaled(_, signal, core)) => {
                    break format!(
                        "signal {signal}{}",
                        if core { " (core dumped)" } else { "" }
                    )
                }
                Err(error) => break format!("exit status unavailable ({error})"),
                _ => {}
            }
            if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                break "exit status not yet available".into();
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let _ = self.drain(stderr);
        let mut message = format!("{reason}; command {status}");
        if let Some(stderr) = self.excerpt() {
            message.push_str(&format!("; stderr: {stderr}"));
        }
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_capture_leaves_child_reaping_to_the_process_owner() {
        let mut child = std::process::Command::new("python3")
            .args(["-c", "raise SystemExit(7)"])
            .spawn()
            .unwrap();
        let pid = Pid::from_raw(child.id() as i32);
        let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
        let deadline = Instant::now() + Duration::from_secs(3);
        while waitid(Id::Pid(pid), flags).unwrap() == WaitStatus::StillAlive {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("status helper did not exit");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let message = StderrCapture::default().failure_details(
            "guest stream closed",
            &child,
            &mut io::Cursor::new(b"error"),
            &AtomicBool::new(false),
        );
        assert!(message.contains("exit status 7"));
        assert_eq!(waitid(Id::Pid(pid), flags), Ok(WaitStatus::Exited(pid, 7)));
        assert_eq!(child.wait().unwrap().code(), Some(7));
    }

    #[test]
    fn sanitizes_terminal_controls_and_unicode_into_one_ascii_line() {
        let mut capture = StderrCapture::default();
        capture.append(
            "\x1b]0;fake title\x07\x1b[31mPermission denied\x1b[0m\n\r\tfile\0\x7f \u{202e}bad"
                .as_bytes(),
        );
        let text = capture.excerpt().unwrap();
        assert!(text.contains("Permission denied file"));
        assert!(text.contains("\\u{202e}bad"));
        assert!(!text.contains("fake title"));
        assert!(!text.contains("[31m"));
        assert!(text.is_ascii() && !text.chars().any(char::is_control));
    }

    #[test]
    fn bounds_retained_bytes_and_output_and_preserves_the_tail() {
        let mut capture = StderrCapture::default();
        capture.append(&vec![b'x'; CAPTURE_LIMIT * 10]);
        capture.append(b"final error");
        assert_eq!(capture.tail.len(), CAPTURE_LIMIT);
        let text = capture.excerpt().unwrap();
        assert!(text.starts_with("[truncated] "));
        assert!(text.ends_with("final error"));
        assert!(text.len() <= LOG_LIMIT + "[truncated] ".len());
    }

    #[test]
    fn stderr_limit_counts_discarded_bytes_across_reads_and_resets_per_connection() {
        let mut capture = StderrCapture::default();
        for _ in 0..4 {
            assert!(!capture
                .drain(&mut io::Cursor::new(vec![b'x'; STDERR_LIMIT / 4]))
                .unwrap());
        }
        assert_eq!(capture.received, STDERR_LIMIT);
        assert_eq!(capture.tail.len(), CAPTURE_LIMIT);
        assert!(capture.drain(&mut io::Cursor::new(b"x")).is_err());
        assert!(!StderrCapture::default()
            .drain(&mut io::Cursor::new(b"x"))
            .unwrap());
    }

    #[test]
    fn draining_rejects_floods_and_does_not_read_more_during_failure_reporting() {
        let mut capture = StderrCapture::default();
        let mut bytes = io::Cursor::new(vec![b'x'; 100_000]);
        assert!(capture
            .drain(&mut bytes)
            .unwrap_err()
            .to_string()
            .contains("stderr limit"));
        let position = bytes.position();
        assert!(position > STDERR_LIMIT as u64 && position <= STDERR_LIMIT as u64 + 4096);
        assert!(capture.drain(&mut bytes).is_err());
        assert_eq!(bytes.position(), position);
        assert_eq!(capture.tail.len(), CAPTURE_LIMIT);
        capture.append(b"\xff\nerror");
        let text = capture.excerpt().unwrap();
        assert!(text.contains("\\u{fffd}"));
        assert!(!text.chars().any(char::is_control));
    }
}
