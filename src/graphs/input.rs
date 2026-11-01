//! The parser owns this file, so validate the opened object and bound the read.
use multiload_qubes_snapshot::MAX_WIRE_BYTES;
use nix::fcntl::OFlag;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Version(u64, u64, i64, i64);

#[cfg(test)]
fn read(path: &Path) -> io::Result<String> {
    read_versioned(path).map(|(text, _)| text)
}

pub(super) fn read_versioned(path: &Path) -> io::Result<(String, Version)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)?;
    // Check the descriptor, not the path: the parser can replace the path
    // between opening it and this check.
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "metrics is not a regular file",
        ));
    }
    let mut text = String::new();
    // Metadata size alone would not bound a file that grows while we read it.
    file.take(MAX_WIRE_BYTES as u64 + 1)
        .read_to_string(&mut text)?;
    if text.len() > MAX_WIRE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "metrics exceeds size limit",
        ));
    }
    Ok((
        text,
        Version(
            metadata.dev(),
            metadata.ino(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "mq-panel-input-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn metrics(&self) -> PathBuf {
            self.0.join("metrics")
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_a_valid_snapshot_and_accepts_atomic_replacement() {
        let dir = Directory::new();
        let now = multiload_qubes_snapshot::unix_time_now().unwrap();
        let wire = format!("MQ2 {now:.6} 25 4000 2000 100 1000 0 0.5 unknown unknown\n");
        fs::write(dir.metrics(), &wire).unwrap();
        let snapshot = crate::Snapshot::decode_fresh(&read(&dir.metrics()).unwrap()).unwrap();
        assert_eq!(snapshot.cpu, Some(25.0));
        fs::write(dir.0.join("replacement"), wire.replace(" 25 ", " 50 ")).unwrap();
        fs::rename(dir.0.join("replacement"), dir.metrics()).unwrap();
        let snapshot = crate::Snapshot::decode_fresh(&read(&dir.metrics()).unwrap()).unwrap();
        assert_eq!(snapshot.cpu, Some(50.0));
    }

    #[test]
    fn bounds_regular_file_reads_and_rejects_invalid_utf8() {
        let dir = Directory::new();
        fs::write(dir.metrics(), vec![b'a'; MAX_WIRE_BYTES]).unwrap();
        assert_eq!(read(&dir.metrics()).unwrap().len(), MAX_WIRE_BYTES);
        fs::write(dir.metrics(), vec![b'a'; MAX_WIRE_BYTES + 1]).unwrap();
        assert_eq!(
            read(&dir.metrics()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::write(dir.metrics(), [0xff]).unwrap();
        assert_eq!(
            read(&dir.metrics()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_symlinks_devices_and_directories() {
        let dir = Directory::new();
        symlink("/dev/zero", dir.metrics()).unwrap();
        assert_eq!(
            read(&dir.metrics()).unwrap_err().raw_os_error(),
            Some(nix::libc::ELOOP)
        );
        assert_eq!(
            read(Path::new("/dev/zero")).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(read(&dir.0).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn fifo_does_not_block() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let dir = Directory::new();
        nix::unistd::mkfifo(
            &dir.metrics(),
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "graphs::input::tests::fifo_child", "--ignored"])
            .env("MQ_FIFO_TEST", dir.metrics())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("opening a FIFO blocked the panel reader");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "run in a child process so a blocking FIFO open cannot hang the test suite"]
    fn fifo_child() {
        let path = std::env::var_os("MQ_FIFO_TEST").unwrap();
        assert_eq!(
            read(Path::new(&path)).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
