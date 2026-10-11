//! XFCE ABI and ownership boundary. The runtime sees only the safe `Host` API.
//!
//! C owns one raw Rc reference from new until free-data. Every other callback
//! borrows an additional Rc so nested GTK dialogs may safely remove the item.
//! The native plugin stays alive throughout free-data; shutdown disconnects the
//! Host before C can destroy it, even if a modal callback still retains Panel.
use super::Panel;
use glib::translate::{from_glib_full, ToGlibPtr};
use gtk::prelude::*;
use std::{
    cell::Cell,
    ffi::{c_char, c_void},
    path::PathBuf,
    ptr::NonNull,
    rc::Rc,
};

/// Native callback table, exported to panel-api.h. All callbacks run on the GTK thread.
#[repr(C)]
pub struct HostApi {
    size: unsafe extern "C" fn(*mut c_void) -> i32,
    vertical: unsafe extern "C" fn(*mut c_void) -> i32,
    // The widget is borrowed for the call; the container retains its own ref.
    attach: unsafe extern "C" fn(*mut c_void, *mut gtk::ffi::GtkWidget),
    // Returns NULL or an owned, g_free-compatible filename.
    path: unsafe extern "C" fn(*mut c_void, i32) -> *mut c_char,
    block: unsafe extern "C" fn(*mut c_void, i32),
}

/// A non-owning GTK-thread host connection. It cannot be copied or constructed
/// outside this module. Detaching makes subsequent calls harmless.
pub(super) struct Host {
    pointer: Cell<Option<NonNull<c_void>>>,
    api: &'static HostApi,
}
impl Host {
    /// # Safety
    /// The pointer and all callbacks are valid on this thread until detach.
    /// The callback table is static and follows the ownership rules above.
    unsafe fn new(pointer: NonNull<c_void>, api: &'static HostApi) -> Self {
        Self {
            pointer: Cell::new(Some(pointer)),
            api,
        }
    }
    pub(super) fn size(&self) -> i32 {
        self.pointer.get().map_or(48, |pointer| {
            // SAFETY: the live connection owns no native data and detach clears
            // the pointer before free-data returns to the native owner.
            unsafe { (self.api.size)(pointer.as_ptr()) }
        })
    }
    pub(super) fn is_vertical(&self) -> bool {
        self.pointer.get().is_some_and(|pointer| {
            // SAFETY: same live GTK-thread connection as size().
            unsafe { (self.api.vertical)(pointer.as_ptr()) != 0 }
        })
    }
    pub(super) fn attach(&self, widget: &impl IsA<gtk::Widget>) {
        if let Some(pointer) = self.pointer.get() {
            // SAFETY: the connection is live; the Rust widget remains borrowed
            // throughout the call, and the native container takes its own ref.
            unsafe {
                (self.api.attach)(pointer.as_ptr(), widget.as_ref().to_glib_none().0);
            }
        }
    }
    pub(super) fn load_path(&self) -> Option<PathBuf> {
        self.path(false)
    }
    pub(super) fn save_path(&self) -> Option<PathBuf> {
        self.path(true)
    }
    fn path(&self, save: bool) -> Option<PathBuf> {
        let pointer = self.pointer.get()?;
        // SAFETY: the live callback returns an owned GLib filename or NULL.
        let path = unsafe { (self.api.path)(pointer.as_ptr(), i32::from(save)) };
        if path.is_null() {
            None
        } else {
            // SAFETY: transfer-full from the callback, freed exactly once here.
            Some(unsafe { from_glib_full(path) })
        }
    }
    pub(super) fn set_menu_blocked(&self, block: bool) {
        if let Some(pointer) = self.pointer.get() {
            // SAFETY: the live host is used only on its GTK thread. The runtime
            // pairs block/unblock operations before disconnecting this Host.
            unsafe {
                (self.api.block)(pointer.as_ptr(), i32::from(block));
            }
        }
    }
    pub(super) fn detach(&self) {
        self.pointer.set(None);
    }
}

/// # Safety
/// The handle is non-NULL, on the GTK thread, and C still owns its Rc reference.
unsafe fn panel_handle(handle: *const c_void) -> Rc<Panel> {
    // SAFETY: borrowing a new Rc does not consume C's owning reference.
    unsafe {
        Rc::increment_strong_count(handle.cast::<Panel>());
        Rc::from_raw(handle.cast::<Panel>())
    }
}

/// # Safety
/// `host` is a live XFCE plugin on the GTK thread, retained through free-data.
/// `api` is a static, complete HostApi with the documented callback contracts.
#[no_mangle]
pub unsafe extern "C" fn mq_panel_new(host: *mut c_void, api: *const HostApi) -> *const c_void {
    if let Err(e) = gtk::init() {
        eprintln!("Cannot initialize GTK: {e}");
        return std::ptr::null();
    }
    let Some(pointer) = NonNull::new(host) else {
        return std::ptr::null();
    };
    if api.is_null() {
        return std::ptr::null();
    }
    // SAFETY: C supplies a live plugin and its static callback table; free-data
    // calls Panel::shutdown, which detaches Host before native destruction.
    let host = unsafe { Host::new(pointer, &*api) };
    Rc::into_raw(Panel::new(Some(host), false)).cast()
}
/// # Safety
/// `handle` is the owning GTK-thread handle returned by new, consumed once.
#[no_mangle]
pub unsafe extern "C" fn mq_panel_free(handle: *const c_void) {
    if !handle.is_null() {
        // SAFETY: consumes exactly the one owning reference held by C.
        let panel = unsafe { Rc::from_raw(handle.cast::<Panel>()) };
        panel.shutdown();
    }
}
/// # Safety
/// `handle` is a live panel handle on the GTK thread, still owned by C.
#[no_mangle]
pub unsafe extern "C" fn mq_panel_save(handle: *const c_void) {
    // SAFETY: the callback borrows the live handle for this operation.
    unsafe { panel_handle(handle) }.save();
}
/// # Safety
/// `handle` is a live panel handle on the GTK thread, still owned by C.
#[no_mangle]
pub unsafe extern "C" fn mq_panel_configure(handle: *const c_void) {
    // SAFETY: the additional Rc keeps the callback alive during modal loops.
    unsafe { panel_handle(handle) }.configure();
}
/// # Safety
/// `handle` is a live panel handle on the GTK thread, still owned by C.
#[no_mangle]
pub unsafe extern "C" fn mq_panel_resize(handle: *const c_void, vertical: i32) {
    // SAFETY: the callback borrows the live handle for this operation.
    let panel = unsafe { panel_handle(handle) };
    // The C ABI uses -1 for a size-only notification; runtime code uses Option.
    panel.size_changed((vertical >= 0).then_some(vertical != 0));
}
#[no_mangle]
pub extern "C" fn mq_panel_about() {
    super::about();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Calls {
        size: Cell<u32>,
        vertical: Cell<u32>,
        paths: Cell<u32>,
        blocks: Cell<u32>,
    }
    unsafe extern "C" fn size(pointer: *mut c_void) -> i32 {
        let calls = unsafe { &*pointer.cast::<Calls>() };
        calls.size.set(calls.size.get() + 1);
        32
    }
    unsafe extern "C" fn vertical(pointer: *mut c_void) -> i32 {
        let calls = unsafe { &*pointer.cast::<Calls>() };
        calls.vertical.set(calls.vertical.get() + 1);
        1
    }
    unsafe extern "C" fn attach(_: *mut c_void, _: *mut gtk::ffi::GtkWidget) {}
    unsafe extern "C" fn path(pointer: *mut c_void, save: i32) -> *mut c_char {
        let calls = unsafe { &*pointer.cast::<Calls>() };
        calls.paths.set(calls.paths.get() + 1);
        let path = if save == 0 {
            std::path::Path::new("/tmp/load.rc")
        } else {
            std::path::Path::new("/tmp/save.rc")
        };
        path.to_glib_full()
    }
    unsafe extern "C" fn block(pointer: *mut c_void, _: i32) {
        let calls = unsafe { &*pointer.cast::<Calls>() };
        calls.blocks.set(calls.blocks.get() + 1);
    }
    static API: HostApi = HostApi {
        size,
        vertical,
        attach,
        path,
        block,
    };
    #[test]
    fn disconnected_host_never_calls_the_native_owner_again() {
        let mut calls = Calls::default();
        // SAFETY: this test owns Calls longer than Host, and API is static.
        let host = unsafe { Host::new(NonNull::from(&mut calls).cast(), &API) };
        assert_eq!(host.size(), 32);
        assert!(host.is_vertical());
        assert_eq!(host.load_path(), Some(PathBuf::from("/tmp/load.rc")));
        assert_eq!(host.save_path(), Some(PathBuf::from("/tmp/save.rc")));
        host.set_menu_blocked(true);
        host.set_menu_blocked(false);
        host.detach();
        host.detach();
        assert_eq!(host.size(), 48);
        assert!(!host.is_vertical());
        assert_eq!(host.load_path(), None);
        assert_eq!(host.save_path(), None);
        host.set_menu_blocked(true);
        assert_eq!(calls.size.get(), 1);
        assert_eq!(calls.vertical.get(), 1);
        assert_eq!(calls.paths.get(), 2);
        assert_eq!(calls.blocks.get(), 2);
    }
}
