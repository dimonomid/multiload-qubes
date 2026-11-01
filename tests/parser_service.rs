//! Exercises the real parser process through atomic file publication and inotify.
use multiload_qubes::metrics::QubeIdentity;
use multiload_qubes_snapshot::{unix_time_now, Snapshot};
use serde_json::json;
use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn identity(domain_id: u32, uuid: &str) -> QubeIdentity {
    QubeIdentity {
        domain_id,
        uuid: uuid.into(),
    }
}

struct Fixture {
    path: PathBuf,
    child: Option<Child>,
    sequence: Cell<u64>,
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
        Self {
            path,
            child: None,
            sequence: Cell::new(0),
        }
    }
    fn write(&self, name: &str, bytes: &[u8]) {
        let path = self.path.join(name);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes).unwrap();
        fs::rename(tmp, path).unwrap();
    }
    fn begin(&self, expected: &[&str], wait: f64, memory: bool) -> f64 {
        self.begin_at(now(), expected, wait, memory, 0.5)
    }
    fn begin_at(
        &self,
        timestamp: f64,
        expected: &[&str],
        wait: f64,
        memory: bool,
        period: f64,
    ) -> f64 {
        self.sequence.set(self.sequence.get() + 1);
        let domain = |id| json!({"id":id,"identity":format!("uuid-{id}"),"name":format!("qube-{id}"),"cpu_ns":0,"vcpus":2,"allocated":1000,"paused":false,"shutdown":false,"stubdom":null});
        let expected: std::collections::BTreeMap<_, _> = expected
            .iter()
            .map(|file| (*file, identity(1, "uuid-1")))
            .collect();
        let sample = json!({
            "timestamp":timestamp,"period":period,
            "host":{"total":4000,"free":2000,"cpus":4,"domains":[domain(0),domain(1)]},
            "cpu":25,"qube_cpu":null,"disk":null,
            "dom0_ram":[1000,100],"dom0_swap":[100,50],
        });
        let frame = json!({"tick":{"tick_id":self.tick_id(),"deadline":now()+wait,"expected":expected},"sample":sample,"memory_enabled":memory,"network_identity":identity(1, "uuid-1")});
        self.write("host.json", &serde_json::to_vec(&frame).unwrap());
        timestamp
    }
    fn tick_id(&self) -> String {
        format!("collector-a/{}", self.sequence.get())
    }
    fn raw(&self, file: &str, session: &str, bytes: &[u8]) {
        self.raw_for(
            file,
            &self.tick_id(),
            identity(1, "uuid-1"),
            session,
            now(),
            bytes,
        );
    }
    fn raw_for(
        &self,
        file: &str,
        tick_id: &str,
        identity: QubeIdentity,
        session: &str,
        timestamp: f64,
        bytes: &[u8],
    ) {
        self.write(&format!("raw/{file}"),&serde_json::to_vec(&json!({"tick_id":tick_id,"identity":identity,"session":session,"timestamp":timestamp,"payload":bytes})).unwrap());
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
    fn wait(&self, stamp: f64) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(s) = fs::read_to_string(self.path.join("metrics"))
                .map_err(|e| e.to_string())
                .and_then(|s| Snapshot::decode(&s).map_err(|e| e.to_string()))
            {
                if (s.timestamp - stamp).abs() < 0.00001 {
                    return s;
                }
            }
            assert!(
                Instant::now() < deadline,
                "parser output did not reach expected tick"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn version(&self) -> std::time::SystemTime {
        fs::metadata(self.path.join("metrics"))
            .unwrap()
            .modified()
            .unwrap()
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
const MEMORY: &[u8] = b"M2 1 1000 400 500 125";
const NETWORK: &[u8] = b"N1 1000000000 12345678-1234-1234-1234-123456789abc 1:100:200";

#[test]
fn publishes_once_when_all_reports_arrive_without_waiting_for_deadline() {
    let mut f = Fixture::new();
    let stamp = f.begin(&["memory-1", "network"], 2.0, true);
    // A fast reply may be on disk before the parser reads the manifest.
    f.raw("memory-1", "a", MEMORY);
    f.start();
    std::thread::sleep(Duration::from_millis(40));
    assert!(
        !f.path.join("metrics").exists(),
        "must wait for the other requested report"
    );
    let sent = Instant::now();
    f.raw("network", "a", NETWORK);
    let s = f.wait(stamp);
    assert!(
        sent.elapsed() < Duration::from_secs(1),
        "must not wait for the two-second test deadline"
    );
    assert_eq!(s.used, 700);
    assert_eq!(s.swap.unwrap().entries[1].used, 375);
    let version = f.version();
    f.raw("memory-1", "a", b"M2 1 1000 100 500 125");
    f.raw("network", "a", NETWORK);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        f.version(),
        version,
        "completed ticks must not be republished"
    );
    f.stop();
}

#[test]
fn deadline_publishes_partial_data_and_late_replies_do_not_change_it() {
    let mut f = Fixture::new();
    let start = Instant::now();
    let stamp = f.begin(&["memory-1", "network"], 0.1, true);
    let old_tick_id = f.tick_id();
    f.raw("memory-1", "a", MEMORY);
    f.start();
    let s = f.wait(stamp);
    assert!(start.elapsed() >= Duration::from_millis(90));
    assert_eq!(s.used, 700);
    assert!(s.network.is_none());
    let version = f.version();
    f.raw("network", "a", NETWORK);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(f.version(), version);
    let next = f.begin(&["network"], 0.1, true);
    f.raw_for(
        "network",
        &old_tick_id,
        identity(1, "uuid-1"),
        "a",
        now(),
        NETWORK,
    );
    assert!(
        f.wait(next).network.is_none(),
        "an old reply cannot satisfy the next tick"
    );
    f.stop();
}

#[test]
fn validates_reports_recovers_from_bad_files_and_preserves_network_baselines() {
    let mut f = Fixture::new();
    let stamp = f.begin(&["network"], 0.1, true);
    f.raw("network", "a", NETWORK);
    f.start();
    assert!(f.wait(stamp).network.is_none());
    let report = |stamp, rx| format!("N1 {stamp} 12345678-1234-1234-1234-123456789abc 1:{rx}:200");
    let stamp = f.begin(&["network"], 0.1, true);
    f.raw("network", "a", report(1_500_000_000_u64, 600).as_bytes());
    assert_eq!(f.wait(stamp).network, Some((1000.0, 0.0)));
    let stamp = f.begin(&["network"], 0.1, true);
    f.raw("network", "b", report(2_000_000_000_u64, 1100).as_bytes());
    assert!(f.wait(stamp).network.is_none());
    let stamp = f.begin(&["network"], 0.1, true);
    f.raw("network", "b", report(2_500_000_000_u64, 1600).as_bytes());
    assert_eq!(f.wait(stamp).network, Some((1000.0, 0.0)));
    for bytes in [b"M2 999 1000 400 500 125".as_slice(), &[0xff, 0, 0x1b]] {
        let stamp = f.begin(&["memory-1"], 0.1, true);
        f.raw("memory-1", "a", bytes);
        assert_eq!(f.wait(stamp).unknown, 1000);
    }
    for wrong_identity in [identity(1, "old-incarnation"), identity(2, "uuid-1")] {
        let stamp = f.begin(&["memory-1"], 0.1, true);
        f.raw_for("memory-1", &f.tick_id(), wrong_identity, "a", now(), MEMORY);
        assert_eq!(f.wait(stamp).unknown, 1000);
    }
    let stamp = f.begin(&["network"], 0.1, true);
    f.write("raw/network", &vec![b'x'; 70 * 1024]);
    assert!(f.wait(stamp).network.is_none());
    let stamp = f.begin(&["memory-1"], 0.1, true);
    f.raw("memory-1", "a", MEMORY);
    assert_eq!(f.wait(stamp).used, 700);
    f.stop();
    f.start();
    assert_eq!(f.wait(stamp).used, 700);
    f.stop();
}

#[test]
fn empty_manifests_publish_immediately_and_stale_hosts_are_removed() {
    let mut f = Fixture::new();
    let stamp = f.begin(&[], 2.0, false);
    f.start();
    let s = f.wait(stamp);
    assert_eq!(s.unknown, 1000);
    f.begin_at(now() - 60.0, &[], 0.1, false, 0.5);
    let deadline = Instant::now() + Duration::from_secs(3);
    while f.path.join("metrics").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!f.path.join("metrics").exists());
    let stamp = f.begin(&[], 0.1, false);
    f.wait(stamp);
    f.stop();
}

#[test]
fn typed_host_measurements_and_guest_replies_make_one_snapshot() {
    let mut f = Fixture::new();
    let stamp = f.begin(&["memory-1"], 1.0, true);
    let mut frame: serde_json::Value =
        serde_json::from_slice(&fs::read(f.path.join("host.json")).unwrap()).unwrap();
    frame["sample"]["cpu"] = json!(37.5);
    frame["sample"]["disk"] = json!({"devices":["sda"],"rates":[4096.0,1024.0]});
    frame["sample"]["qube_cpu"] = json!({
        "window_ns":1_000_000_000,"host_cpus":4,
        "usage":{"expected":2,"entries":[{"id":"1/uuid-1","name":"qube-1","total":2_000_000_000_u64,"used":1_000_000_000_u64}]},
    });
    f.write("host.json", &serde_json::to_vec(&frame).unwrap());
    f.raw("memory-1", "a", MEMORY);
    f.start();
    let result = f.wait(stamp);
    assert!((result.timestamp - stamp).abs() < 0.000001);
    assert_eq!(result.period, 0.5);
    assert_eq!(result.cpu, Some(37.5));
    assert_eq!(result.disk.unwrap().rates, Some((4096.0, 1024.0)));
    let cpu = result.qube_cpu.unwrap();
    assert_eq!(cpu.window_ns, 1_000_000_000);
    assert_eq!(cpu.usage.entries[0].percent(), Some(50.0));
    assert_eq!(result.used, 700);
    assert_eq!(result.swap.unwrap().entries[1].used, 375);
    f.stop();
}

#[test]
fn invalid_typed_host_samples_remove_output_and_can_recover() {
    let mut f = Fixture::new();
    let stamp = f.begin(&[], 0.1, true);
    f.start();
    f.wait(stamp);
    for (field, value) in [
        ("cpu", json!(101.0)),
        ("period", json!(0.0)),
        ("dom0_ram", json!([100, 101])),
        ("disk", json!({"devices":["sda","sda"],"rates":[0.0,0.0]})),
        (
            "qube_cpu",
            json!({"window_ns":0,"host_cpus":4,"usage":{"expected":1,"entries":[]}}),
        ),
    ] {
        f.begin(&[], 0.1, true);
        let mut frame: serde_json::Value =
            serde_json::from_slice(&fs::read(f.path.join("host.json")).unwrap()).unwrap();
        frame["sample"][field] = value;
        f.write("host.json", &serde_json::to_vec(&frame).unwrap());
        let deadline = Instant::now() + Duration::from_secs(3);
        while f.path.join("metrics").exists() {
            assert!(Instant::now() < deadline, "invalid {field} was accepted");
            std::thread::sleep(Duration::from_millis(5));
        }
        let stamp = f.begin(&[], 0.1, true);
        f.wait(stamp);
    }
    f.stop();
}
