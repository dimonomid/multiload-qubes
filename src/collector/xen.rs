//! Opaque libxenstat/XenStore handles: no Xen-version-specific struct layouts.
use super::{Domain, Host};
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr, CString};
use std::ptr::NonNull;

#[link(name = "dl")]
extern "C" {
    fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}
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

struct Library(NonNull<c_void>);
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            dlclose(self.0.as_ptr());
        }
    }
}
impl Library {
    fn open() -> Result<Self, String> {
        let mut candidates = Vec::new();
        // Runtime library sonames carry the Xen version. Resolve the installed
        // library's stable opaque API rather than linking to the build host's version.
        for dir in ["/usr/lib64", "/usr/lib", "/lib64", "/lib"] {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("libxenstat.so.")
                    && name[14..].bytes().all(|b| b.is_ascii_digit() || b == b'.')
                {
                    candidates.push(entry.path());
                }
            }
        }
        candidates.sort();
        candidates.dedup();
        for path in candidates {
            let path = CString::new(path.as_os_str().as_encoded_bytes())
                .map_err(|_| "invalid library path")?;
            // RTLD_NOW | RTLD_LOCAL. Only system-owned library directories above.
            if let Some(ptr) = NonNull::new(unsafe { dlopen(path.as_ptr(), 2) }) {
                return Ok(Self(ptr));
            }
        }
        Err("cannot load libxenstat; install xen-libs in dom0".into())
    }
    unsafe fn symbol<T: Copy>(&self, name: &str) -> Result<T, String> {
        let name = CString::new(name).map_err(|_| "invalid symbol")?;
        let ptr = dlsym(self.0.as_ptr(), name.as_ptr());
        if ptr.is_null() {
            return Err(format!("missing Xen API {}", name.to_string_lossy()));
        }
        // Callers supply the exact function signatures from xenstat.h. POSIX
        // specifies dlsym conversion to a function pointer; sizes match on Linux.
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
        Ok(std::mem::transmute_copy(&ptr))
    }
}

type Init = unsafe extern "C" fn() -> *mut c_void;
type Release = unsafe extern "C" fn(*mut c_void);
type GetNode = unsafe extern "C" fn(*mut c_void, c_uint) -> *mut c_void;
type Count = unsafe extern "C" fn(*mut c_void) -> c_uint;
type Quantity = unsafe extern "C" fn(*mut c_void) -> u64;
type ByIndex = unsafe extern "C" fn(*mut c_void, c_uint) -> *mut c_void;
type Name = unsafe extern "C" fn(*mut c_void) -> *const c_char;

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
    memory: Quantity,
    paused: Count,
    shutdown: Count,
    dying: Count,
}
impl Api {
    unsafe fn load(lib: &Library) -> Result<Self, String> {
        Ok(Self {
            init: lib.symbol("xenstat_init")?,
            uninit: lib.symbol("xenstat_uninit")?,
            get_node: lib.symbol("xenstat_get_node")?,
            free_node: lib.symbol("xenstat_free_node")?,
            total: lib.symbol("xenstat_node_tot_mem")?,
            free: lib.symbol("xenstat_node_free_mem")?,
            cpus: lib.symbol("xenstat_node_num_cpus")?,
            count: lib.symbol("xenstat_node_num_domains")?,
            domain: lib.symbol("xenstat_node_domain_by_index")?,
            id: lib.symbol("xenstat_domain_id")?,
            name: lib.symbol("xenstat_domain_name")?,
            cpu: lib.symbol("xenstat_domain_cpu_ns")?,
            memory: lib.symbol("xenstat_domain_cur_mem")?,
            paused: lib.symbol("xenstat_domain_paused")?,
            shutdown: lib.symbol("xenstat_domain_shutdown")?,
            dying: lib.symbol("xenstat_domain_dying")?,
        })
    }
}

pub struct Xen {
    api: Api,
    handle: NonNull<c_void>,
    xs: NonNull<c_void>,
    _library: Library,
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
        let library = Library::open()?;
        let api = unsafe { Api::load(&library)? };
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
            _library: library,
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
        // Flags 0 request base domain/host data, not extra network/VBD/vCPU scans.
        let ptr = NonNull::new(unsafe { (self.api.get_node)(self.handle.as_ptr(), 0) })
            .ok_or("cannot read Xen statistics")?;
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
    #[test]
    fn resolves_native_api_without_opening_privileged_handles() {
        let lib = Library::open().unwrap();
        unsafe {
            Api::load(&lib).unwrap();
            assert!(lib.symbol::<Init>("not_a_real_xen_function").is_err());
        }
    }
}
