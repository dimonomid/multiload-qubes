//! Preferences and named-theme editing. Signal handlers hold weak Rust owners.
use super::{
    config::{Appearance, Selection, CHART_KINDS, CHART_NAMES},
    themes::{reserved_name, valid_name, ThemeStore, BUILTINS},
    Panel,
};
use gtk::{prelude::*, ResponseType};
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

pub(super) struct Preferences {
    owner: Weak<Panel>,
    pub dialog: gtk::Dialog,
    pub(super) notebook: gtk::Notebook,
    pub(super) width: gtk::SpinButton,
    pub(super) interval: gtk::SpinButton,
    pub(super) enabled: [gtk::CheckButton; 7],
    pub(super) theme: gtk::ComboBoxText,
    pub(super) chart: gtk::ComboBoxText,
    pub(super) colors: [gtk::ColorButton; 7],
    labels: [gtk::Label; 7],
    pub(super) border: gtk::SpinButton,
    pub(super) transparent: gtk::CheckButton,
    preview: gtk::DrawingArea,
    demo: RefCell<crate::Graphs>,
    pub(super) editor: RefCell<Appearance>,
    pub(super) store: RefCell<ThemeStore>,
    updating: Cell<bool>,
    accepted: Cell<bool>,
    pub(super) closed: Cell<bool>,
    status: gtk::Label,
    pub(super) save_as: gtk::Button,
    pub(super) save_changes: gtk::Button,
    pub(super) rename: gtk::Button,
    pub(super) delete: gtk::Button,
}
fn grid() -> gtk::Grid {
    gtk::Grid::builder()
        .border_width(12)
        .row_spacing(6)
        .column_spacing(12)
        .build()
}
fn label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .halign(gtk::Align::Start)
        .build()
}
fn rgba(value: u32) -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::new(
        (value >> 24) as f64 / 255.,
        ((value >> 16) & 255) as f64 / 255.,
        ((value >> 8) & 255) as f64 / 255.,
        (value & 255) as f64 / 255.,
    )
}
fn packed(c: gtk::gdk::RGBA) -> u32 {
    [c.red(), c.green(), c.blue(), c.alpha()]
        .into_iter()
        .fold(0, |out, v| (out << 8) | (v * 255.).round() as u32)
}
impl Preferences {
    pub fn new(owner: &Rc<Panel>) -> Rc<Self> {
        let config = owner.config.borrow().clone();
        let (store, errors) = ThemeStore::load();
        let mut editor = config.appearance.clone();
        editor.validate_selection(&store);
        let dialog = gtk::Dialog::with_buttons(
            Some("Multiload for Qubes"),
            None::<&gtk::Window>,
            gtk::DialogFlags::DESTROY_WITH_PARENT,
            &[
                ("_Cancel", ResponseType::Cancel),
                ("_Apply", ResponseType::Ok),
            ],
        );
        let theme = gtk::ComboBoxText::new();
        let chart = gtk::ComboBoxText::new();
        for name in CHART_NAMES {
            chart.append_text(name);
        }
        chart.set_active(Some(0));
        let width = gtk::SpinButton::with_range(0., 512., 1.);
        width.set_value(config.width as f64);
        let interval = gtk::SpinButton::with_range(250., 10000., 250.);
        interval.set_value(config.interval as f64);
        let enabled = std::array::from_fn(|i| {
            let button = gtk::CheckButton::with_label(CHART_NAMES[i]);
            button.set_active(config.enabled[i]);
            button
        });
        let mut demo = crate::Graphs::new();
        for _ in 0..400 {
            demo.update(true);
        }
        let prefs = Rc::new(Self {
            owner: Rc::downgrade(owner),
            dialog,
            notebook: gtk::Notebook::new(),
            width,
            interval,
            enabled,
            theme,
            chart,
            colors: std::array::from_fn(|_| {
                let c = gtk::ColorButton::new();
                c.set_use_alpha(true);
                c
            }),
            labels: [
                "Border",
                "Background top",
                "Background bottom",
                "Usage",
                "",
                "Unknown usage",
                "Missing data",
            ]
            .map(label),
            border: gtk::SpinButton::with_range(0., 16., 1.),
            transparent: gtk::CheckButton::with_label("Transparent background"),
            preview: gtk::DrawingArea::new(),
            demo: RefCell::new(demo),
            editor: RefCell::new(editor),
            store: RefCell::new(store),
            updating: Cell::new(false),
            accepted: Cell::new(false),
            closed: Cell::new(false),
            status: label(if errors.is_empty() {
                "Theme files are saved immediately. Apply saves the panel appearance."
            } else {
                "Some theme files could not be loaded (invalid format, duplicate name or file error)."
            }),
            save_as: gtk::Button::with_label("Save as…"),
            save_changes: gtk::Button::with_label("Save changes"),
            rename: gtk::Button::with_label("Rename…"),
            delete: gtk::Button::with_label("Delete…"),
        });
        for error in errors {
            eprintln!("Cannot load custom theme: {error}");
        }
        prefs.build();
        prefs.connect();
        prefs.rebuild_themes();
        prefs
    }
    fn build(&self) {
        let general = grid();
        general.attach(&label("Chart width (0 = square)"), 0, 0, 1, 1);
        general.attach(&self.width, 1, 0, 1, 1);
        general.attach(&label("Update interval (ms)"), 0, 1, 1, 1);
        general.attach(&self.interval, 1, 1, 1, 1);
        for (i, button) in self.enabled.iter().enumerate() {
            general.attach(
                button,
                (i % 2) as i32,
                2 + (i / 2) as i32,
                if i == 6 { 2 } else { 1 },
                1,
            );
        }
        general.attach(&label("Per-qube charts: 100% = one domain's capacity.\nColors can be changed in Appearance."), 0, 6, 2, 1);
        let appearance = grid();
        appearance.attach(&label("Theme (applies to all charts)"), 0, 0, 1, 1);
        appearance.attach(&self.theme, 1, 0, 2, 1);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for button in [
            &self.save_as,
            &self.save_changes,
            &self.rename,
            &self.delete,
        ] {
            actions.pack_start(button, false, false, 0);
        }
        appearance.attach(&actions, 0, 1, 3, 1);
        appearance.attach(&label("Chart"), 0, 2, 1, 1);
        appearance.attach(&self.chart, 1, 2, 2, 1);
        for f in 0..7 {
            appearance.attach(&self.labels[f], 0, 3 + f as i32, 1, 1);
            appearance.attach(&self.colors[f], 1, 3 + f as i32, 1, 1);
        }
        let border_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        border_box.pack_start(&self.border, false, false, 0);
        border_box.pack_start(&label("px (0 = none)"), false, false, 0);
        appearance.attach(&border_box, 2, 3, 1, 1);
        appearance.attach(&self.transparent, 0, 11, 3, 1);
        self.preview.set_size_request(320, 64);
        appearance.attach(&self.preview, 0, 12, 3, 1);
        let reset = gtk::Button::with_label("Reset all appearance settings");
        // Keep a widget name for the isolated GTK integration checks.
        reset.set_widget_name("reset-appearance");
        appearance.attach(&reset, 0, 13, 3, 1);
        self.status.set_line_wrap(true);
        appearance.attach(&self.status, 0, 14, 3, 1);
        self.notebook
            .append_page(&general, Some(&gtk::Label::new(Some("General"))));
        self.notebook
            .append_page(&appearance, Some(&gtk::Label::new(Some("Appearance"))));
        self.dialog.content_area().add(&self.notebook);
    }
    fn connect(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.dialog.connect_response(move |_, response| {
            if let Some(p) = weak.upgrade() {
                if response == ResponseType::Ok {
                    p.apply();
                }
                p.close();
            }
        });
        let weak = Rc::downgrade(self);
        self.dialog.connect_destroy(move |_| {
            let Some(p) = weak.upgrade() else {
                return;
            };
            p.closed.set(true);
            if let Some(owner) = p.owner.upgrade() {
                if !owner.closed.get() {
                    if !p.accepted.get() {
                        owner.graphs.borrow_mut().palette =
                            owner.config.borrow().appearance.palettes;
                        owner.area.queue_draw();
                    }
                    owner.block_menu(false);
                }
                owner.preferences.borrow_mut().take();
            }
        });
        let weak = Rc::downgrade(self);
        self.chart.connect_changed(move |_| {
            if let Some(p) = weak.upgrade().filter(|p| !p.updating.get()) {
                p.refresh();
            }
        });
        let weak = Rc::downgrade(self);
        self.theme.connect_changed(move |combo| {
            if let Some(p) = weak.upgrade().filter(|p| !p.updating.get()) {
                if let Some(id) = combo.active_id() {
                    p.select(id.as_str());
                }
            }
        });
        for (f, color) in self.colors.iter().enumerate() {
            let weak = Rc::downgrade(self);
            color.connect_color_set(move |color| {
                if let Some(p) = weak.upgrade().filter(|p| !p.updating.get()) {
                    let k = p.chart_kind();
                    {
                        let mut editor = p.editor.borrow_mut();
                        editor.palettes[k].colors[f] = packed(color.rgba());
                        editor.edit();
                    }
                    p.refresh();
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.border.connect_value_changed(move |spin| {
            if let Some(p) = weak.upgrade().filter(|p| !p.updating.get()) {
                {
                    let mut editor = p.editor.borrow_mut();
                    editor.palettes[p.chart_kind()].border = spin.value_as_int() as u32;
                    editor.edit();
                }
                p.refresh();
            }
        });
        let weak = Rc::downgrade(self);
        self.transparent.connect_toggled(move |button| {
            if let Some(p) = weak.upgrade().filter(|p| !p.updating.get()) {
                {
                    let mut editor = p.editor.borrow_mut();
                    let colors = &mut editor.palettes[p.chart_kind()].colors;
                    for value in &mut colors[1..=2] {
                        *value = (*value & !255) | if button.is_active() { 0 } else { 255 };
                    }
                    editor.edit();
                }
                p.refresh();
            }
        });
        for (button, action) in [
            (&self.save_as, Self::save_as_theme as fn(&Self)),
            (&self.save_changes, Self::save_theme_changes),
            (&self.rename, Self::rename_theme),
            (&self.delete, Self::delete_theme),
        ] {
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(p) = weak.upgrade() {
                    action(&p);
                }
            });
        }
        if let Some(reset) = self
            .notebook
            .nth_page(Some(1))
            .and_then(|w| w.downcast::<gtk::Grid>().ok())
            .and_then(|g| g.child_at(0, 13))
            .and_then(|w| w.downcast::<gtk::Button>().ok())
        {
            let weak = Rc::downgrade(self);
            reset.connect_clicked(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.select("0");
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.preview.connect_draw(move |widget, cr| {
            if let Some(p) = weak.upgrade() {
                let (w, h) = (widget.allocated_width(), widget.allocated_height());
                for y in (0..h).step_by(8) {
                    for x in (0..w).step_by(8) {
                        let shade = if (x / 8 + y / 8) % 2 == 0 { 0.9 } else { 0.75 };
                        cr.set_source_rgb(shade, shade, shade);
                        cr.rectangle(x as f64, y as f64, 8., 8.);
                        let _ = cr.fill();
                    }
                }
                let kind = p.chart_kind();
                let on = CHART_KINDS.map(|k| k == kind);
                super::draw_graphs(&p.demo.borrow(), cr, w, h, on, false);
            }
            glib::Propagation::Proceed
        });
    }
    pub fn show(&self) {
        self.dialog.show_all();
        self.refresh();
    }
    pub fn close(&self) {
        unsafe {
            self.dialog.destroy();
        }
    }
    fn chart_kind(&self) -> usize {
        CHART_KINDS[self.chart.active().unwrap_or(0) as usize]
    }
    pub(super) fn refresh(&self) {
        if self.closed.get() {
            return;
        }
        self.updating.set(true);
        let editor = self.editor.borrow();
        let k = self.chart_kind();
        let palette = editor.palettes[k];
        self.labels[3].set_text(match k {
            1 => "Used",
            2 => "Incoming",
            4 => "Read",
            3 | 5 | 6 => "Base line",
            _ => "Usage",
        });
        self.labels[4].set_text(match k {
            1 => "Allocated spare",
            2 => "Outgoing",
            4 => "Write",
            _ => "Maximum fill",
        });
        for f in 0..7 {
            self.colors[f].set_rgba(&rgba(palette.colors[f]));
            self.colors[f].set_title(&self.labels[f].text());
            let visible = !(f == 4 && k == 0 || f == 5 && k != 1);
            self.labels[f].set_visible(visible);
            self.colors[f].set_visible(visible);
        }
        self.border.set_value(palette.border as f64);
        self.transparent
            .set_active(palette.colors[1] & 255 == 0 && palette.colors[2] & 255 == 0);
        self.theme.set_active_id(Some(&editor.selection.id()));
        let source = editor
            .selection
            .source()
            .is_some_and(|id| self.store.borrow().find(id).is_some());
        self.save_changes.set_sensitive(editor.unsaved() && source);
        let saved = matches!(editor.selection, Selection::Saved(_)) && source;
        self.rename.set_sensitive(saved);
        self.delete.set_sensitive(saved);
        self.demo.borrow_mut().palette = editor.palettes;
        if let Some(owner) = self.owner.upgrade().filter(|p| !p.closed.get()) {
            owner.graphs.borrow_mut().palette = editor.palettes;
            owner.area.queue_draw();
        }
        self.preview.queue_draw();
        self.updating.set(false);
    }
    fn rebuild_themes(&self) {
        self.updating.set(true);
        self.theme.remove_all();
        for (builtin, name) in BUILTINS {
            self.theme.append(Some(&(builtin as u32).to_string()), name);
        }
        for theme in &self.store.borrow().themes {
            self.theme
                .append(Some(&format!("saved:{}", theme.id)), &theme.name);
        }
        self.theme.append(Some("1000"), "Custom draft");
        self.updating.set(false);
    }
    fn apply(&self) {
        let Some(owner) = self.owner.upgrade().filter(|p| !p.closed.get()) else {
            return;
        };
        let mut config = owner.config.borrow().clone();
        config.width = self.width.value_as_int();
        config.interval = self.interval.value_as_int() as u32;
        config.enabled = std::array::from_fn(|i| self.enabled[i].is_active());
        config.appearance = self.editor.borrow().clone();
        config.normalize();
        *owner.config.borrow_mut() = config;
        self.accepted.set(true);
        owner.resize();
        owner.restart_timer();
        owner.save();
    }
    fn confirm(&self, text: &str, cancel: &str, accept: &str) -> bool {
        let dialog = gtk::MessageDialog::new(
            Some(&self.dialog),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            gtk::MessageType::Question,
            gtk::ButtonsType::None,
            text,
        );
        dialog.add_buttons(&[
            (cancel, ResponseType::Cancel),
            (accept, ResponseType::Accept),
        ]);
        dialog.set_default_response(ResponseType::Cancel);
        let response = dialog.run();
        unsafe {
            dialog.destroy();
        }
        !self.closed.get() && response == ResponseType::Accept
    }
    fn error(&self, text: &str) {
        if self.closed.get() {
            return;
        }
        let dialog = gtk::MessageDialog::new(
            Some(&self.dialog),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            gtk::MessageType::Error,
            gtk::ButtonsType::Close,
            text,
        );
        dialog.run();
        unsafe {
            dialog.destroy();
        }
    }
    pub(super) fn select(&self, id: &str) {
        if id == "1000" {
            self.editor.borrow_mut().edit();
            self.refresh();
            return;
        }
        let unsaved = self.editor.borrow().unsaved();
        if unsaved
            && !self.confirm(
                "Your changes aren't saved. Discard them?",
                "Keep editing",
                "Discard",
            )
        {
            self.refresh();
            return;
        }
        if let Some(id) = id.strip_prefix("saved:") {
            let theme = self.store.borrow().find(id).cloned();
            if let Some(theme) = theme {
                *self.editor.borrow_mut() = Appearance {
                    selection: Selection::Saved(theme.id),
                    palettes: theme.palettes,
                };
            }
        } else if let Some(theme) = id
            .parse::<u32>()
            .ok()
            .and_then(crate::palette::BuiltinTheme::from_id)
        {
            *self.editor.borrow_mut() = Appearance::builtin(theme);
        }
        self.refresh();
    }
    fn name_prompt(&self, title: &str, initial: &str, choices: bool) -> Option<String> {
        let dialog = gtk::Dialog::with_buttons(
            Some(title),
            Some(&self.dialog),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            &[
                ("Cancel", ResponseType::Cancel),
                ("Save", ResponseType::Accept),
            ],
        );
        let combo = gtk::ComboBoxText::with_entry();
        if choices {
            for theme in &self.store.borrow().themes {
                combo.append_text(&theme.name);
            }
        }
        let entry = combo
            .child()
            .and_then(|w| w.downcast::<gtk::Entry>().ok())?;
        entry.set_widget_name("theme-name-entry");
        entry.set_max_length(128);
        entry.set_text(initial);
        entry.set_activates_default(true);
        dialog.set_default_response(ResponseType::Accept);
        dialog.content_area().set_border_width(12);
        dialog.content_area().add(&label("Theme name"));
        dialog.content_area().add(&combo);
        dialog.show_all();
        let response = dialog.run();
        let name = if response == ResponseType::Accept && !self.closed.get() {
            Some(entry.text().trim().to_owned())
        } else {
            None
        };
        unsafe {
            dialog.destroy();
        }
        let name = name?;
        if !valid_name(&name) || reserved_name(&name) {
            self.error(if reserved_name(&name) {
                "Built-in theme names and Custom draft are reserved."
            } else {
                "Enter a theme name of 1–128 characters without control characters."
            });
            return None;
        }
        Some(name)
    }
    fn overwrite(&self, name: &str) -> bool {
        self.confirm(&format!("Overwrite theme ‘{name}’?"), "Cancel", "Overwrite")
    }
    fn source(&self) -> Option<super::themes::SavedTheme> {
        self.editor
            .borrow()
            .selection
            .source()
            .and_then(|id| self.store.borrow().find(id).cloned())
    }
    fn write(&self, id: Option<&str>, name: &str) -> bool {
        let palettes = self.editor.borrow().palettes;
        let result = self.store.borrow_mut().write(id, name, palettes);
        match result {
            Ok(id) => {
                self.editor.borrow_mut().selection = Selection::Saved(id);
                self.rebuild_themes();
                self.refresh();
                true
            }
            Err(e) => {
                self.error(&e);
                false
            }
        }
    }
    fn save_as_theme(&self) {
        let source = self.source();
        let Some(name) = self.name_prompt(
            "Save theme as…",
            source.as_ref().map_or("", |s| s.name.as_str()),
            true,
        ) else {
            return;
        };
        let existing = self.store.borrow().by_name(&name).cloned();
        if existing.is_some() && !self.overwrite(&name) {
            return;
        }
        self.write(existing.as_ref().map(|t| t.id.as_str()), &name);
    }
    fn save_theme_changes(&self) {
        let source = self.source();
        if let Some(source) = source {
            if self.editor.borrow().unsaved() && self.overwrite(&source.name) {
                self.write(Some(&source.id), &source.name);
            }
        }
    }
    fn rename_theme(&self) {
        if !matches!(self.editor.borrow().selection, Selection::Saved(_)) {
            return;
        }
        let Some(source) = self.source() else {
            return;
        };
        let Some(name) = self.name_prompt("Rename theme", &source.name, false) else {
            return;
        };
        if name == source.name {
            return;
        }
        if self.store.borrow().by_name(&name).is_some() {
            self.error("A theme with that name already exists.");
            return;
        }
        self.write(Some(&source.id), &name);
    }
    fn delete_theme(&self) {
        if !matches!(self.editor.borrow().selection, Selection::Saved(_)) {
            return;
        }
        let Some(source) = self.source() else {
            return;
        };
        if !self.confirm(
            &format!("Delete theme ‘{}’?", source.name),
            "Cancel",
            "Delete",
        ) {
            return;
        }
        let result = self.store.borrow_mut().delete(&source.id);
        match result {
            Ok(()) => {
                self.editor.borrow_mut().selection = Selection::Draft { source: None };
                self.rebuild_themes();
                self.refresh();
            }
            Err(e) => self.error(&e),
        }
    }
}
