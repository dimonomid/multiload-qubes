//! Opaque libxenstat/XenStore handles: no Xen-version-specific struct layouts.
use crate::metrics::{Domain, Host};
use libloading::os::unix::{Library as UnixLibrary, RTLD_LOCAL, RTLD_NOW};
use libloading::Library;
use std::ffi::{c_char, c_uint, c_ulong, c_ulonglong, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

#[link(name = "xenstore")]
extern "C" {
    fn xs_open(flags: c_ulong) -> *mut c_void;
    fn xs_close(xs: *mut c_void);
    fn xs_read(
        xs: *mut c_void,
        transaction: u32,
        path: *const c_char,
        len: *mut c_uint,
    ) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

const LIBRARY_DIRS: [&str; 4] = ["/usr/lib64", "/usr/lib", "/lib64", "/lib"];

fn candidates(directories: &[&Path]) -> Vec<PathBuf> {
    let mut paths = std::collections::BTreeSet::new();
    for directory in directories {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(version) = name.strip_prefix("libxenstat.so.") else {
                continue;
            };
            if !version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            let path = entry.path();
            // Deduplicate /lib aliases and version symlinks. Preserve broken
            // paths so loading errors can explain missing files/dependencies.
            paths.insert(std::fs::canonicalize(&path).unwrap_or(path));
        }
    }
    paths.into_iter().collect()
}

// Function signatures match the public Xen xenstat.h API. Quantities are
// unsigned long long (bytes or nanoseconds); handles and nodes remain opaque.
// These pointers never leave Api/Node/Xen. Api owns the library, and Node borrows
// Api, so the library stays loaded through every call and native-resource Drop.
type Init = unsafe extern "C" fn() -> *mut c_void;
type Release = unsafe extern "C" fn(*mut c_void);
type GetNode = unsafe extern "C" fn(*mut c_void, c_uint) -> *mut c_void;
type Count = unsafe extern "C" fn(*mut c_void) -> c_uint;
type Quantity = unsafe extern "C" fn(*mut c_void) -> c_ulonglong;
type ByIndex = unsafe extern "C" fn(*mut c_void, c_uint) -> *mut c_void;
type Name = unsafe extern "C" fn(*mut c_void) -> *mut c_char;

struct Api {
    init: Init,
    uninit: Release,
    get_node: GetNode,
    free_node: Release,
    total: Quantity,
    free: Quantity,
    cpus: Count,
    count: Count,
    domain: ByIndex,
    id: Count,
    name: Name,
    cpu: Quantity,
    vcpus: Count,
    vcpu: ByIndex,
    vcpu_online: Count,
    memory: Quantity,
    paused: Count,
    shutdown: Count,
    dying: Count,
    _library: Library,
}
impl Api {
    fn open() -> Result<Self, String> {
        let directories: Vec<_> = LIBRARY_DIRS.iter().map(Path::new).collect();
        let paths = candidates(&directories);
        if paths.is_empty() {
            return Err(format!(
                "no versioned libxenstat found in {}; install xen-libs in dom0",
                LIBRARY_DIRS.join(", ")
            ));
        }
        // System-owned directories supply the trusted Xen implementations.
        unsafe { Self::open_candidates(&paths) }
    }

    /// Candidates must be trusted native code. Any Xen symbols they provide must
    /// match the public xenstat.h ABI; missing symbols are rejected before use.
    unsafe fn open_candidates(paths: &[PathBuf]) -> Result<Self, String> {
        let mut errors = Vec::new();
        for path in paths {
            // Resolve dependencies immediately and keep symbols local, as before.
            let result = UnixLibrary::open(Some(path), RTLD_NOW | RTLD_LOCAL)
                .map_err(|e| e.to_string())
                .and_then(|library| Self::load(library.into()));
            match result {
                Ok(api) => return Ok(api),
                Err(error) => errors.push(format!("{}: {error}", path.display())),
            }
        }
        Err(format!(
            "no compatible libxenstat could be loaded:\n{}",
            errors.join("\n")
        ))
    }

    unsafe fn online_vcpus(&self, domain: *mut c_void) -> Option<u32> {
        let slots = (self.vcpus)(domain);
        if slots > 65536 {
            return None;
        }
        let mut online = 0;
        for index in 0..slots {
            let vcpu = (self.vcpu)(domain, index);
            if vcpu.is_null() {
                return None;
            }
            online += ((self.vcpu_online)(vcpu) != 0) as u32;
        }
        Some(online)
    }

    /// Any present Xen symbols must match the public C ABI. Missing symbols are
    /// rejected; as with all FFI, C signatures cannot be checked at runtime.
    unsafe fn load(library: Library) -> Result<Self, String> {
        macro_rules! symbol {
            ($name:literal, $signature:ty) => {
                library
                    .get::<$signature>(concat!($name, "\0").as_bytes())
                    .map(|symbol| *symbol)
                    .map_err(|error| format!("{}: {error}", $name))?
            };
        }
        Ok(Self {
            init: symbol!("xenstat_init", Init),
            uninit: symbol!("xenstat_uninit", Release),
            get_node: symbol!("xenstat_get_node", GetNode),
            free_node: symbol!("xenstat_free_node", Release),
            total: symbol!("xenstat_node_tot_mem", Quantity),
            free: symbol!("xenstat_node_free_mem", Quantity),
            cpus: symbol!("xenstat_node_num_cpus", Count),
            count: symbol!("xenstat_node_num_domains", Count),
            domain: symbol!("xenstat_node_domain_by_index", ByIndex),
            id: symbol!("xenstat_domain_id", Count),
            name: symbol!("xenstat_domain_name", Name),
            cpu: symbol!("xenstat_domain_cpu_ns", Quantity),
            vcpus: symbol!("xenstat_domain_num_vcpus", Count),
            vcpu: symbol!("xenstat_domain_vcpu", ByIndex),
            vcpu_online: symbol!("xenstat_vcpu_online", Count),
            memory: symbol!("xenstat_domain_cur_mem", Quantity),
            paused: symbol!("xenstat_domain_paused", Count),
            shutdown: symbol!("xenstat_domain_shutdown", Count),
            dying: symbol!("xenstat_domain_dying", Count),
            _library: library,
        })
    }
}

pub struct Xen {
    api: Api,
    handle: NonNull<c_void>,
    xs: NonNull<c_void>,
    vcpu_unavailable: bool,
}
impl Drop for Xen {
    fn drop(&mut self) {
        unsafe {
            xs_close(self.xs.as_ptr());
            (self.api.uninit)(self.handle.as_ptr());
        }
    }
}
struct Node<'a> {
    ptr: NonNull<c_void>,
    api: &'a Api,
}
impl Drop for Node<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.api.free_node)(self.ptr.as_ptr());
        }
    }
}

impl Xen {
    pub fn open() -> Result<Self, String> {
        let api = Api::open()?;
        let handle =
            NonNull::new(unsafe { (api.init)() }).ok_or("Xen access requires dom0 privileges")?;
        let Some(xs) = NonNull::new(unsafe { xs_open(0) }) else {
            unsafe {
                (api.uninit)(handle.as_ptr());
            }
            return Err("cannot open XenStore in dom0".into());
        };
        Ok(Self {
            api,
            handle,
            xs,
            vcpu_unavailable: false,
        })
    }
    fn read(&self, path: &str) -> Option<String> {
        let path = CString::new(path).ok()?;
        let mut len = 0;
        let value = unsafe { xs_read(self.xs.as_ptr(), 0, path.as_ptr(), &mut len) };
        if value.is_null() {
            return None;
        }
        let text = if len <= 4096 {
            // xs_read returns an allocated buffer with exactly len readable bytes.
            std::str::from_utf8(unsafe {
                std::slice::from_raw_parts(value.cast::<u8>(), len as usize)
            })
            .ok()
            .map(str::to_owned)
        } else {
            None
        };
        unsafe {
            free(value);
        }
        text
    }
    pub fn sample(&mut self) -> Result<Host, String> {
        // XENSTAT_VCPU (1) collects online state for every vCPU slot. If that
        // scan fails, retain the original base metrics and leave Qube CPU unknown.
        let detailed = NonNull::new(unsafe { (self.api.get_node)(self.handle.as_ptr(), 1) });
        let has_vcpus = detailed.is_some();
        let ptr = detailed
            .or_else(|| NonNull::new(unsafe { (self.api.get_node)(self.handle.as_ptr(), 0) }))
            .ok_or("cannot read Xen statistics")?;
        let unavailable = !has_vcpus;
        if unavailable != self.vcpu_unavailable {
            eprintln!(
                "{}",
                if has_vcpus {
                    "Xen vCPU metrics recovered"
                } else {
                    "Xen vCPU metrics unavailable; using base host/domain statistics"
                }
            );
        }
        self.vcpu_unavailable = unavailable;
        let node = Node {
            ptr,
            api: &self.api,
        };
        let p = node.ptr.as_ptr();
        let count = unsafe { (self.api.count)(p) };
        if count > 65536 {
            return Err("too many Xen domains".into());
        }
        let mut domains = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for i in 0..count {
            let d = unsafe { (self.api.domain)(p, i) };
            if d.is_null() {
                return Err("missing Xen domain".into());
            }
            if unsafe { (self.api.dying)(d) } != 0 {
                continue;
            }
            let id = unsafe { (self.api.id)(d) };
            if !seen.insert(id) {
                continue;
            }
            let name_ptr = unsafe { (self.api.name)(d) };
            let name = if name_ptr.is_null() {
                String::new()
            } else {
                // Borrowed library string remains valid until Node is freed.
                unsafe { CStr::from_ptr(name_ptr) }
                    .to_str()
                    .unwrap_or("")
                    .to_owned()
            };
            domains.push(Domain {
                id,
                identity: self
                    .read(&format!("/local/domain/{id}/vm"))
                    .unwrap_or_default(),
                name,
                cpu_ns: unsafe { (self.api.cpu)(d) },
                vcpus: if has_vcpus {
                    unsafe { self.api.online_vcpus(d) }
                } else {
                    None
                },
                allocated: unsafe { (self.api.memory)(d) } / 1024,
                paused: unsafe { (self.api.paused)(d) } != 0,
                shutdown: unsafe { (self.api.shutdown)(d) } != 0,
                stubdom: self
                    .read(&format!("/local/domain/{id}/image/device-model-domid"))
                    .and_then(|s| s.parse().ok()),
            });
        }
        Ok(Host {
            total: unsafe { (self.api.total)(p) } / 1024,
            free: unsafe { (self.api.free)(p) } / 1024,
            cpus: unsafe { (self.api.cpus)(p) },
            domains,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::symlink;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "mq-xen-loader-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn library(&self, name: &str, missing: Option<&str>) -> PathBuf {
            // Definitions compile against the actual public header, not copied C
            // structs. An unload marker verifies ownership on failure and success.
            let marker = self.0.join(format!("{name}.unloaded"));
            let mut source = format!("#include <xenstat.h>\n#include <stdio.h>\n#include <stdint.h>\nstatic unsigned freed;\n__attribute__((destructor)) static void unloaded(void) {{ FILE *f=fopen({marker:?},\"w\"); if(f) fclose(f); }}\nunsigned test_freed(void) {{ return freed; }}\n");
            for (symbol, definition) in [
                ("xenstat_init", "xenstat_handle *xenstat_init(void) { return (xenstat_handle *)(uintptr_t)1; }"),
                ("xenstat_uninit", "void xenstat_uninit(xenstat_handle *h) {}"),
                ("xenstat_get_node", "xenstat_node *xenstat_get_node(xenstat_handle *h, unsigned int flags) { return (xenstat_node *)(uintptr_t)2; }"),
                ("xenstat_free_node", "void xenstat_free_node(xenstat_node *n) { freed++; }"),
                ("xenstat_node_tot_mem", "unsigned long long xenstat_node_tot_mem(xenstat_node *n) { return 4194304ULL; }"),
                ("xenstat_node_free_mem", "unsigned long long xenstat_node_free_mem(xenstat_node *n) { return 1048576ULL; }"),
                ("xenstat_node_num_cpus", "unsigned xenstat_node_num_cpus(xenstat_node *n) { return 8; }"),
                ("xenstat_node_num_domains", "unsigned xenstat_node_num_domains(xenstat_node *n) { return 1; }"),
                ("xenstat_node_domain_by_index", "xenstat_domain *xenstat_node_domain_by_index(xenstat_node *n, unsigned index) { return (xenstat_domain *)(uintptr_t)3; }"),
                ("xenstat_domain_id", "unsigned xenstat_domain_id(xenstat_domain *d) { return 23; }"),
                ("xenstat_domain_name", "char *xenstat_domain_name(xenstat_domain *d) { static char name[]=\"dev\"; return name; }"),
                ("xenstat_domain_num_vcpus", "unsigned xenstat_domain_num_vcpus(xenstat_domain *d) { return 3; }"),
                ("xenstat_domain_vcpu", "xenstat_vcpu *xenstat_domain_vcpu(xenstat_domain *d, unsigned i) { return (xenstat_vcpu *)(uintptr_t)(10+i); }"),
                ("xenstat_vcpu_online", "unsigned xenstat_vcpu_online(xenstat_vcpu *v) { return (uintptr_t)v != 11; }"),
                ("xenstat_domain_cpu_ns", "unsigned long long xenstat_domain_cpu_ns(xenstat_domain *d) { return 1234567890123ULL; }"),
                ("xenstat_domain_cur_mem", "unsigned long long xenstat_domain_cur_mem(xenstat_domain *d) { return 2097152ULL; }"),
                ("xenstat_domain_paused", "unsigned xenstat_domain_paused(xenstat_domain *d) { return 0; }"),
                ("xenstat_domain_shutdown", "unsigned xenstat_domain_shutdown(xenstat_domain *d) { return 0; }"),
                ("xenstat_domain_dying", "unsigned xenstat_domain_dying(xenstat_domain *d) { return 0; }"),
            ] {
                if missing != Some(symbol) { source.push_str(definition); source.push('\n'); }
            }
            let path = self.0.join(name);
            let mut compiler = Command::new("cc")
                .args(["-shared", "-fPIC", "-x", "c", "-", "-o"])
                .arg(&path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            compiler
                .stdin
                .take()
                .unwrap()
                .write_all(source.as_bytes())
                .unwrap();
            let output = compiler.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_installed_api_without_opening_privileged_handles() {
        Api::open().unwrap();
    }

    #[test]
    fn fallback_releases_incomplete_libraries_and_keeps_selected_api_alive() {
        let fixture = Fixture::new();
        let corrupt = fixture.0.join("corrupt.so");
        std::fs::write(&corrupt, "not a shared library").unwrap();
        let incomplete = fixture.library("incomplete.so", Some("xenstat_domain_dying"));
        let good = fixture.library("good.so", None);
        // The fixtures and signatures above implement the trusted xenstat.h ABI.
        let api = unsafe { Api::open_candidates(&[corrupt, incomplete, good]) }.unwrap();
        assert!(fixture.0.join("incomplete.so.unloaded").exists());
        assert!(!fixture.0.join("good.so.unloaded").exists());
        unsafe {
            assert_eq!((api.total)(std::ptr::null_mut()), 4194304);
            assert_eq!((api.cpus)(std::ptr::null_mut()), 8);
            let handle = (api.init)();
            let node = Node {
                ptr: NonNull::new((api.get_node)(handle, 0)).unwrap(),
                api: &api,
            };
            let domain = (api.domain)(node.ptr.as_ptr(), 0);
            assert_eq!((api.id)(domain), 23);
            assert_eq!((api.cpu)(domain), 1234567890123);
            assert_eq!(api.online_vcpus(domain), Some(2));
            assert_eq!(CStr::from_ptr((api.name)(domain)).to_str().unwrap(), "dev");
            drop(node);
            let freed = api
                ._library
                .get::<unsafe extern "C" fn() -> c_uint>(b"test_freed\0")
                .unwrap();
            assert_eq!(freed(), 1);
            (api.uninit)(handle);
        }
        drop(api);
        assert!(fixture.0.join("good.so.unloaded").exists());
    }

    #[test]
    fn errors_identify_paths_and_missing_symbols() {
        let fixture = Fixture::new();
        let corrupt = fixture.0.join("corrupt.so");
        std::fs::write(&corrupt, "not ELF").unwrap();
        let missing = fixture.library("missing.so", Some("xenstat_domain_dying"));
        let absent = fixture.0.join("absent.so");
        let error =
            unsafe { Api::open_candidates(&[absent.clone(), corrupt.clone(), missing.clone()]) }
                .err()
                .unwrap();
        assert!(error.contains(absent.to_str().unwrap()));
        assert!(error.contains(corrupt.to_str().unwrap()));
        assert!(error.contains(missing.to_str().unwrap()));
        assert!(error.contains("xenstat_domain_dying"));
        assert!(error.contains("no compatible libxenstat"));
        assert!(fixture.0.join("missing.so.unloaded").exists());
    }

    #[test]
    fn discovery_filters_and_deduplicates_version_symlinks() {
        let fixture = Fixture::new();
        let actual = fixture.library("libxenstat.so.4.20.0", None);
        symlink(&actual, fixture.0.join("libxenstat.so.4.20")).unwrap();
        for name in [
            "libxenstat.so",
            "libxenstat.so.4.debug",
            "libxenstat.so.",
            "libxenstat.so.4..20",
        ] {
            std::fs::write(fixture.0.join(name), "ignored").unwrap();
        }
        assert_eq!(
            candidates(&[fixture.0.as_path(), fixture.0.as_path()]),
            vec![actual]
        );
        assert!(candidates(&[&fixture.0.join("absent")]).is_empty());
    }
}
