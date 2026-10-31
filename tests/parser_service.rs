//! Exercises the real parser process through atomic file publication and inotify.
use multiload_qubes_snapshot::{unix_time_now, Snapshot};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Fixture {
    path: PathBuf,
    child: Option<Child>,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mq-parser-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(path.join("raw")).unwrap();
        Self { path, child: None }
    }
    fn write(&self, name: &str, bytes: &[u8]) {
        let path = self.path.join(name);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes).unwrap();
        fs::rename(tmp, path).unwrap();
    }
    fn host(&self, timestamp: f64, memory: bool) {
        let domain = |id| json!({"id":id,"identity":format!("uuid-{id}"),"name":format!("qube-{id}"),"cpu_ns":0,"vcpus":2,"allocated":1000,"paused":false,"shutdown":false,"stubdom":null});
        let metrics = format!("MQ2 {timestamp:.6} 25 4000 2000 100 1000 0 0.5 unknown unknown\n");
        let frame = json!({"host":{"total":4000,"free":2000,"cpus":4,"domains":[domain(0),domain(1)]},"metrics":metrics,"memory_enabled":memory,"network_identity":"1/uuid-1","dom0_ram":[1000,100],"dom0_swap":[100,50]});
        self.write("host.json", &serde_json::to_vec(&frame).unwrap());
    }
    fn raw(&self, file: &str, identity: &str, session: &str, timestamp: f64, bytes: &[u8]) {
        self.write(&format!("raw/{file}"), &serde_json::to_vec(&json!({"identity":identity,"session":session,"timestamp":timestamp,"payload":bytes})).unwrap());
    }
    fn start(&mut self) {
        let _ = fs::remove_file(self.path.join("metrics"));
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_multiload-qubes-parser"))
                .arg("--input")
                .arg(&self.path)
                .arg("--output")
                .arg(self.path.join("metrics"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGTERM,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if child.try_wait().unwrap().is_none() {
                let _ = child.kill();
                panic!("parser did not stop");
            }
            assert!(child.wait().unwrap().success());
        }
    }
    #[track_caller]
    fn wait(&self, condition: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(snapshot) = fs::read_to_string(self.path.join("metrics"))
                .map_err(|e| e.to_string())
                .and_then(|s| Snapshot::decode(&s).map_err(|e| e.to_string()))
            {
                if condition(&snapshot) {
                    return snapshot;
                }
            }
            assert!(
                Instant::now() < deadline,
                "parser output did not reach expected state"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}
fn now() -> f64 {
    unix_time_now().unwrap()
}

#[test]
fn parses_atomic_updates_checks_identity_expires_reports_and_recovers_after_restart() {
    let mut f = Fixture::new();
    f.host(now(), true);
    f.start();
    f.wait(|s| s.unknown == 1000);
    f.raw(
        "memory-1",
        "1/uuid-1",
        "connection-a",
        now(),
        b"M2 1 1000 400 500 125",
    );
    let s = f.wait(|s| s.used == 700);
    assert_eq!(s.swap.unwrap().entries[1].used, 375);
    assert_eq!(s.qube_ram.unwrap().entries[1].used, 600);
    f.raw(
        "memory-1",
        "1/uuid-1",
        "connection-a",
        now(),
        b"M2 999 1000 400 500 125",
    );
    f.wait(|s| s.unknown == 1000);
    f.raw(
        "memory-1",
        "1/uuid-1",
        "connection-a",
        now(),
        b"M2 1 1000 400 500 125",
    );
    f.wait(|s| s.used == 700);
    f.raw(
        "memory-1",
        "1/old-incarnation",
        "connection-a",
        now(),
        b"M2 1 1000 400 500 125",
    );
    f.wait(|s| s.unknown == 1000);
    f.raw(
        "memory-1",
        "1/uuid-1",
        "connection-a",
        now(),
        b"M2 1 1000 400 500 125",
    );
    f.wait(|s| s.used == 700);
    f.raw(
        "memory-1",
        "1/uuid-1",
        "connection-a",
        now() - 10.0,
        b"M2 1 1000 400 500 125",
    );
    f.wait(|s| s.unknown == 1000);
    f.raw(
        "memory-1",
        "1/uuid-1",
        "connection-a",
        now(),
        b"M2 1 1000 400 500 125",
    );
    f.wait(|s| s.used == 700);
    f.stop();
    f.start();
    f.wait(|s| s.used == 700);
    f.host(now(), false);
    f.wait(|s| s.unknown == 1000);
    f.host(now() - 60.0, false);
    let deadline = Instant::now() + Duration::from_secs(3);
    while f.path.join("metrics").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !f.path.join("metrics").exists(),
        "stale host must not be republished as fresh"
    );
    f.host(now(), true);
    f.wait(|s| s.used == 700);
    f.stop();
}

#[test]
fn network_reconnects_reset_the_baseline_and_malformed_files_do_not_crash_parser() {
    let mut f = Fixture::new();
    f.host(now(), true);
    f.start();
    f.wait(|s| s.network.is_none());
    let boot = "12345678-1234-1234-1234-123456789abc";
    let report = |stamp, rx| format!("N1 {stamp} {boot} 1:{rx}:200");
    f.raw(
        "network",
        "1/uuid-1",
        "a",
        now(),
        report(1_000_000_000_u64, 100).as_bytes(),
    );
    // A new host timestamp acknowledges a rescan of the baseline file.
    let ack = now();
    f.host(ack, true);
    f.wait(|s| (s.timestamp - ack).abs() < 0.00001);
    f.raw(
        "network",
        "1/uuid-1",
        "a",
        now(),
        report(1_500_000_000_u64, 600).as_bytes(),
    );
    f.wait(|s| s.network == Some((1000.0, 0.0)));
    f.raw(
        "network",
        "1/uuid-1",
        "b",
        now(),
        report(2_000_000_000_u64, 1100).as_bytes(),
    );
    f.wait(|s| s.network.is_none());
    f.raw(
        "network",
        "1/uuid-1",
        "b",
        now(),
        report(2_500_000_000_u64, 1600).as_bytes(),
    );
    f.wait(|s| s.network == Some((1000.0, 0.0)));
    f.write("raw/network", &vec![b'x'; 70 * 1024]);
    f.wait(|s| s.network.is_none());
    f.raw("memory-1", "1/uuid-1", "a", now(), &[0xff, 0, 0x1b]);
    f.wait(|s| s.unknown == 1000);
    f.raw("memory-1", "1/uuid-1", "a", now(), b"M2 1 1000 400 500 125");
    f.wait(|s| s.used == 700);
    f.stop();
}
