//! GTK3 panel runtime. Only XFCE-specific calls cross the small native adapter.
mod config;
mod preferences;
mod preview;
mod themes;

use config::{Config, CHART_KINDS};
use glib::translate::{from_glib_full, ToGlibPtr};
use gtk::prelude::*;
use std::{
    cell::{Cell, RefCell},
    ffi::{c_char, c_void},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

/// Non-owning XFCE calls supplied by the native adapter. This avoids linking
/// XFCE into the Rust library, its unit tests, or the collector.
#[repr(C)]
pub struct HostApi {
    size: unsafe extern "C" fn(*mut c_void) -> i32,
    vertical: unsafe extern "C" fn(*mut c_void) -> i32,
    attach: unsafe extern "C" fn(*mut c_void, *mut gtk::ffi::GtkWidget),
    path: unsafe extern "C" fn(*mut c_void, i32) -> *mut c_char,
    block: unsafe extern "C" fn(*mut c_void, i32),
}
#[derive(Clone, Copy)]
struct Host {
    pointer: *mut c_void,
    api: &'static HostApi,
}

pub(super) struct Panel {
    host: Option<Host>,
    area: gtk::DrawingArea,
    graphs: RefCell<crate::Graphs>,
    config: RefCell<Config>,
    vertical: Cell<bool>,
    timer: RefCell<Option<glib::SourceId>>,
    preferences: RefCell<Option<Rc<preferences::Preferences>>>,
    closed: Cell<bool>,
    menu_blocked: Cell<bool>,
    demo: bool,
}
impl Panel {
    fn new(host: Option<Host>, demo: bool) -> Rc<Self> {
        let panel = Rc::new(Self {
            host,
            area: gtk::DrawingArea::new(),
            graphs: RefCell::new(crate::Graphs::new()),
            config: RefCell::new(Config::default()),
            vertical: Cell::new(false),
            timer: RefCell::new(None),
            preferences: RefCell::new(None),
            closed: Cell::new(false),
            menu_blocked: Cell::new(false),
            demo,
        });
        if let Some(host) = host {
            panel
                .vertical
                .set(unsafe { (host.api.vertical)(host.pointer) != 0 });
            if let Some(path) = panel.path(false) {
                match Config::load(&path) {
                    Ok(config) => *panel.config.borrow_mut() = config,
                    Err(e) => eprintln!("Cannot load Multiload settings: {e}"),
                }
            }
        }
        panel.graphs.borrow_mut().palette = panel.config.borrow().appearance.palettes;
        panel
            .area
            .add_events(gtk::gdk::EventMask::BUTTON_PRESS_MASK);
        panel.area.set_has_tooltip(true);
        let weak = Rc::downgrade(&panel);
        panel.area.connect_draw(move |widget, context| {
            if let Some(p) = weak.upgrade() {
                draw_graphs(
                    &p.graphs.borrow(),
                    context,
                    widget.allocated_width(),
                    widget.allocated_height(),
                    p.config.borrow().enabled,
                    p.vertical.get(),
                );
            }
            glib::Propagation::Proceed
        });
        let weak = Rc::downgrade(&panel);
        panel
            .area
            .connect_query_tooltip(move |widget, x, y, keyboard, tooltip| {
                let Some(p) = weak.upgrade() else {
                    return false;
                };
                if keyboard {
                    return false;
                }
                let Some((kind, rect)) = chart_at(
                    p.config.borrow().enabled,
                    p.vertical.get(),
                    widget.allocated_width(),
                    widget.allocated_height(),
                    x,
                    y,
                ) else {
                    return false;
                };
                let mut graphs = p.graphs.borrow_mut();
                let text = unsafe {
                    crate::mq_tooltip(&mut *graphs, kind as i32, rect.width(), rect.height())
                };
                if text.is_null() {
                    return false;
                }
                let text = unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy();
                tooltip.set_text(Some(&text));
                tooltip.set_tip_area(&rect);
                true
            });
        if let Some(host) = host {
            unsafe {
                (host.api.attach)(
                    host.pointer,
                    panel.area.upcast_ref::<gtk::Widget>().to_glib_none().0,
                );
            }
        }
        panel.resize();
        panel.tick();
        panel.restart_timer();
        panel.area.show();
        panel
    }
    fn path(&self, save: bool) -> Option<PathBuf> {
        let host = self.host?;
        let path = unsafe { (host.api.path)(host.pointer, i32::from(save)) };
        if path.is_null() {
            None
        } else {
            Some(unsafe { from_glib_full(path) })
        }
    }
    fn block_menu(&self, block: bool) {
        if self.closed.get() || self.menu_blocked.replace(block) == block {
            return;
        }
        if let Some(host) = self.host {
            unsafe {
                (host.api.block)(host.pointer, i32::from(block));
            }
        }
    }
    fn save(&self) {
        if let Some(path) = self.path(true) {
            if let Err(e) = self.config.borrow().save(&path) {
                eprintln!("Cannot save Multiload settings: {e}");
            }
        }
    }
    fn tick(&self) {
        self.graphs.borrow_mut().update(self.demo);
        self.area.trigger_tooltip_query();
        self.area.queue_draw();
    }
    fn resize(&self) {
        let size = self
            .host
            .map_or(48, |host| unsafe { (host.api.size)(host.pointer) });
        let config = self.config.borrow();
        let width = if config.width == 0 {
            size
        } else {
            config.width
        };
        let count = config.enabled.iter().filter(|v| **v).count() as i32;
        let (w, h) = if self.vertical.get() {
            (size, width * count)
        } else {
            (width * count, size)
        };
        self.area.set_size_request(w, h);
    }
    fn restart_timer(self: &Rc<Self>) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local(
            Duration::from_millis(self.config.borrow().interval as u64),
            move || {
                if let Some(p) = weak.upgrade().filter(|p| !p.closed.get()) {
                    p.tick();
                    glib::ControlFlow::Continue
                } else {
                    glib::ControlFlow::Break
                }
            },
        );
        *self.timer.borrow_mut() = Some(timer);
    }
    fn configure(self: &Rc<Self>) {
        let existing = self.preferences.borrow().clone();
        if let Some(prefs) = existing {
            prefs.dialog.present();
            return;
        }
        self.block_menu(true);
        let prefs = preferences::Preferences::new(self);
        *self.preferences.borrow_mut() = Some(prefs.clone());
        prefs.show();
    }
    fn shutdown(&self) {
        self.block_menu(false);
        self.closed.set(true);
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        let prefs = self.preferences.borrow_mut().take();
        if let Some(prefs) = prefs {
            prefs.close();
        }
    }
}
impl Drop for Panel {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.get_mut().take() {
            timer.remove();
        }
    }
}
fn draw_graphs(
    g: &crate::Graphs,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    enabled: [bool; 7],
    vertical: bool,
) {
    unsafe {
        crate::mq_draw(
            g,
            cr.to_raw_none() as *mut c_void,
            width,
            height,
            enabled[0].into(),
            enabled[1].into(),
            enabled[2].into(),
            enabled[3].into(),
            enabled[4].into(),
            enabled[5].into(),
            enabled[6].into(),
            vertical.into(),
        );
    }
}
fn chart_at(
    enabled: [bool; 7],
    vertical: bool,
    width: i32,
    height: i32,
    x: i32,
    y: i32,
) -> Option<(usize, gtk::gdk::Rectangle)> {
    let count = enabled.iter().filter(|v| **v).count() as i32;
    if count == 0 || x < 0 || y < 0 || x >= width || y >= height {
        return None;
    }
    let (w, h) = if vertical {
        (width, height / count)
    } else {
        (width / count, height)
    };
    if w < 4 || h < 4 {
        return None;
    }
    let index = if vertical { y / h } else { x / w };
    let kind = CHART_KINDS
        .into_iter()
        .zip(enabled)
        .filter_map(|(kind, on)| on.then_some(kind))
        .nth(index as usize)?;
    Some((
        kind,
        gtk::gdk::Rectangle::new(
            if vertical { 0 } else { index * w },
            if vertical { index * h } else { 0 },
            w,
            h,
        ),
    ))
}

// C owns one Rc reference until free-data. Each callback borrows a fresh Rc so
// removal during a nested GTK dialog cannot invalidate the Rust callback stack.
unsafe fn panel_handle(handle: *const Panel) -> Rc<Panel> {
    Rc::increment_strong_count(handle);
    Rc::from_raw(handle)
}
/// # Safety
/// `host` is a live XFCE plugin on the GTK thread, retained until mq_panel_free.
/// `api` points to the adapter’s static callback table for the panel lifetime.
#[no_mangle]
pub unsafe extern "C" fn mq_panel_new(host: *mut c_void, api: *const HostApi) -> *const c_void {
    if let Err(e) = gtk::init() {
        eprintln!("Cannot initialize GTK: {e}");
        return std::ptr::null();
    }
    if host.is_null() || api.is_null() {
        return std::ptr::null();
    }
    Rc::into_raw(Panel::new(
        Some(Host {
            pointer: host,
            api: &*api,
        }),
        false,
    ))
    .cast()
}
/// # Safety
/// `handle` is the live GTK-thread handle returned by mq_panel_new, freed once.
#[no_mangle]
pub unsafe extern "C" fn mq_panel_free(handle: *const c_void) {
    if !handle.is_null() {
        let panel = Rc::from_raw(handle.cast::<Panel>());
        panel.shutdown();
    }
}
/// # Safety
/// `handle` is a live panel handle on the GTK thread.
#[no_mangle]
pub unsafe extern "C" fn mq_panel_save(handle: *const c_void) {
    panel_handle(handle.cast()).save();
}
/// # Safety
/// `handle` is a live panel handle on the GTK thread.
#[no_mangle]
pub unsafe extern "C" fn mq_panel_configure(handle: *const c_void) {
    panel_handle(handle.cast()).configure();
}
/// # Safety
/// `handle` is a live panel handle on the GTK thread.
#[no_mangle]
pub unsafe extern "C" fn mq_panel_resize(handle: *const c_void, vertical: i32) {
    let panel = panel_handle(handle.cast());
    if vertical >= 0 {
        panel.vertical.set(vertical != 0);
    }
    panel.resize();
}
#[no_mangle]
pub extern "C" fn mq_panel_about() {
    gtk::AboutDialog::builder()
        .program_name("Multiload for Qubes")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("CPU, RAM, network, swap and disk I/O graphs.\nPalette from multiload-ng.")
        .license_type(gtk::License::Gpl20)
        .build()
        .show();
}
