//! GTK3 panel runtime. Only XFCE-specific calls cross the small native adapter.
mod config;
mod ffi;
mod preferences;
pub(crate) mod preview;
mod themes;
mod updates;

use crate::charts::ChartKind;
use config::Config;
use ffi::Host;
use gtk::prelude::*;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

pub(super) struct Panel {
    host: Option<Host>,
    area: gtk::DrawingArea,
    graphs: RefCell<crate::Graphs>,
    config: RefCell<Config>,
    vertical: Cell<bool>,
    timer: RefCell<Option<glib::SourceId>>,
    updates: RefCell<Option<Rc<updates::Feed>>>,
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
            updates: RefCell::new(None),
            preferences: RefCell::new(None),
            closed: Cell::new(false),
            menu_blocked: Cell::new(false),
            demo,
        });
        if let Some(host) = &panel.host {
            panel.vertical.set(host.is_vertical());
            if let Some(path) = host.load_path() {
                match Config::load(&path) {
                    Ok(config) => *panel.config.borrow_mut() = config,
                    Err(e) => eprintln!("Cannot load Multiload settings: {e}"),
                }
            }
        }
        panel.graphs.borrow_mut().palette = panel.config.borrow().appearance.palettes;
        // XFCE uses button events from this drawing area to open its menu.
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
                    &p.config.borrow().visible_kinds(),
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
                    &p.config.borrow().visible_kinds(),
                    p.vertical.get(),
                    widget.allocated_width(),
                    widget.allocated_height(),
                    x,
                    y,
                ) else {
                    return false;
                };
                let graphs = p.graphs.borrow();
                let text = graphs.tooltip(kind, rect.width(), rect.height());
                tooltip.set_text(Some(&text));
                tooltip.set_tip_area(&rect);
                true
            });
        if let Some(host) = &panel.host {
            host.attach(&panel.area);
        }
        panel.resize();
        if demo {
            panel.tick();
            panel.start_demo_timer();
        } else {
            let weak = Rc::downgrade(&panel);
            match updates::Feed::start(
                std::path::Path::new("/run/multiload-qubes/metrics"),
                move |sample| {
                    if let Some(panel) = weak.upgrade().filter(|p| !p.closed.get()) {
                        panel.graphs.borrow_mut().push_sample(sample, false);
                        panel.redraw();
                    }
                },
            ) {
                Ok(feed) => *panel.updates.borrow_mut() = Some(feed),
                Err(error) => eprintln!("Cannot watch Multiload metrics: {error}"),
            }
        }
        panel.area.show();
        panel
    }
    fn block_menu(&self, block: bool) {
        if self.closed.get() || self.menu_blocked.replace(block) == block {
            return;
        }
        if let Some(host) = &self.host {
            host.set_menu_blocked(block);
        }
    }
    fn save_config(&self, config: &Config) -> Result<(), String> {
        // The standalone preview has no panel settings file to persist.
        let Some(host) = &self.host else {
            return Ok(());
        };
        let path = host
            .save_path()
            .ok_or("Cannot determine the panel settings file location.")?;
        config
            .save(&path)
            .map_err(|error| format!("{}: {error}", path.display()))
    }
    fn save(&self) -> Result<(), String> {
        self.save_config(&self.config.borrow())
    }
    fn tick(&self) {
        self.graphs.borrow_mut().update(self.demo);
        self.redraw();
    }
    fn redraw(&self) {
        self.area.trigger_tooltip_query();
        self.area.queue_draw();
    }
    fn resize(&self) {
        let size = self.host.as_ref().map_or(48, Host::size);
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
    fn size_changed(&self, vertical: Option<bool>) {
        if let Some(vertical) = vertical {
            self.vertical.set(vertical);
        }
        self.resize();
    }
    fn start_demo_timer(self: &Rc<Self>) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local(Duration::from_millis(500), move || {
            if let Some(p) = weak.upgrade().filter(|p| !p.closed.get()) {
                p.tick();
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
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
        self.updates.borrow_mut().take();
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        let prefs = self.preferences.borrow_mut().take();
        if let Some(prefs) = prefs {
            prefs.close();
        }
        if let Some(host) = &self.host {
            host.detach();
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
    kinds: &[ChartKind],
    vertical: bool,
) {
    if let Err(error) = g.draw(cr, width, height, kinds, vertical) {
        eprintln!("Cannot draw Multiload graphs: {error}");
    }
}
fn chart_at(
    kinds: &[ChartKind],
    vertical: bool,
    width: i32,
    height: i32,
    x: i32,
    y: i32,
) -> Option<(ChartKind, gtk::gdk::Rectangle)> {
    let count = kinds.len() as i32;
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
    let kind = *kinds.get(index as usize)?;
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

fn about() {
    gtk::AboutDialog::builder()
        .program_name("Multiload for Qubes")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("CPU, RAM, network, swap and disk I/O graphs.\nPalette from multiload-ng.\n\nWritten by Dmitry Frank")
        .website("https://dmitryfrank.com")
        .website_label("https://dmitryfrank.com")
        .license_type(gtk::License::Gpl20)
        .build()
        .show();
}
