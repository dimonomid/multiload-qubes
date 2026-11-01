//! GTK checks run explicitly under Xvfb, with temporary panel settings.
use super::charts::ChartKind;
use super::palette::BuiltinTheme;
use super::{
    config::{Appearance, Config, Selection},
    preferences::Preferences,
    themes::ThemeStore,
    Graphs, Panel,
};
use gtk::prelude::*;
use std::{cell::Cell, rc::Rc};

#[derive(Clone)]
enum Prompt {
    Name(&'static str),
    Message(&'static str, gtk::ResponseType),
}
fn find_entry(widget: &gtk::Widget) -> Option<gtk::Entry> {
    if widget.widget_name() == "theme-name-entry" {
        return widget.clone().downcast().ok();
    }
    widget
        .clone()
        .downcast::<gtk::Container>()
        .ok()?
        .children()
        .iter()
        .find_map(find_entry)
}
fn prompts(steps: Vec<Prompt>) -> Rc<Cell<usize>> {
    let next = Rc::new(Cell::new(0));
    let progress = next.clone();
    glib::idle_add_local(move || {
        for window in gtk::Window::list_toplevels() {
            let Ok(dialog) = window.downcast::<gtk::Dialog>() else {
                continue;
            };
            if !dialog.is_modal() || !dialog.is_visible() {
                continue;
            }
            match &steps[progress.get()] {
                Prompt::Name(name) => {
                    let Some(entry) = find_entry(dialog.upcast_ref()) else {
                        continue;
                    };
                    entry.set_text(name);
                    progress.set(progress.get() + 1);
                    dialog.response(gtk::ResponseType::Accept);
                }
                Prompt::Message(expected, response) => {
                    let Ok(message) = dialog.clone().downcast::<gtk::MessageDialog>() else {
                        continue;
                    };
                    assert!(message.text().unwrap_or_default().contains(expected));
                    progress.set(progress.get() + 1);
                    dialog.response(*response);
                }
            }
            return if progress.get() == steps.len() {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            };
        }
        glib::ControlFlow::Continue
    });
    next
}
fn message(text: &'static str, accept: bool) -> Prompt {
    Prompt::Message(
        text,
        if accept {
            gtk::ResponseType::Accept
        } else {
            gtk::ResponseType::Cancel
        },
    )
}
fn prefs(panel: &Rc<Panel>) -> Rc<Preferences> {
    panel.configure();
    panel.preferences.borrow().as_ref().unwrap().clone()
}
fn emit_color(prefs: &Preferences, f: usize, value: gtk::gdk::RGBA) {
    prefs.colors[f].set_rgba(&value);
    prefs.colors[f].emit_by_name::<()>("color-set", &[]);
}
fn click(button: &gtk::Button, steps: Vec<Prompt>) {
    let count = steps.len();
    let progress = prompts(steps);
    button.emit_clicked();
    assert_eq!(progress.get(), count);
}
fn screenshot(prefs: &Preferences) {
    prefs.notebook.set_current_page(Some(1));
    while gtk::events_pending() {
        gtk::main_iteration();
    }
    // A layout change may be waiting for the next frame, not an event above.
    prefs.dialog.check_resize();
    let surface = cairo::ImageSurface::create(
        cairo::Format::ARgb32,
        prefs.dialog.allocated_width(),
        prefs.dialog.allocated_height(),
    )
    .unwrap();
    let context = cairo::Context::new(&surface).unwrap();
    prefs.dialog.draw(&context);
    surface
        .write_to_png(&mut std::fs::File::create("build/appearance-preferences.png").unwrap())
        .unwrap();
}
fn chart_order_test() {
    let panel = Panel::new(None, true);
    panel.config.borrow_mut().enabled = [true; ChartKind::COUNT];
    let editor = prefs(&panel);
    assert!(!editor.move_up.is_sensitive());
    assert!(!editor.enable_chart.is_sensitive());
    assert!(editor.move_down.is_sensitive());
    for index in [1, 2] {
        editor.move_down.emit_clicked();
        assert_eq!(editor.chart_list.selected_row().unwrap().index(), index);
        assert!(editor.move_down.is_sensitive());
        assert!(editor.move_up.is_sensitive());
    }
    editor.move_up.emit_clicked();
    assert_eq!(editor.chart_list.selected_row().unwrap().index(), 1);
    editor.disable_chart.emit_clicked();
    assert_eq!(editor.available.children().len(), 1);
    assert_eq!(editor.chart_list.children().len(), 6);
    assert!(editor.enable_chart.is_sensitive());
    editor.enable_chart.emit_clicked();
    assert_eq!(editor.chart_list.selected_row().unwrap().index(), 6);
    assert!(!editor.move_down.is_sensitive());
    editor.disable_chart.emit_clicked();
    assert_eq!(panel.config.borrow().order, Config::default().order);
    editor.dialog.response(gtk::ResponseType::Ok);
    assert_eq!(
        panel.config.borrow().order,
        [
            ChartKind::QubeCpu,
            ChartKind::Ram,
            ChartKind::QubeRam,
            ChartKind::Network,
            ChartKind::Swap,
            ChartKind::Disk,
            ChartKind::Cpu
        ]
    );
    assert!(!panel.config.borrow().enabled[ChartKind::Cpu.index()]);
    let kinds = panel.config.borrow().visible_kinds();
    for vertical in [false, true] {
        let (w, h) = if vertical { (48, 288) } else { (288, 48) };
        for (i, &kind) in kinds.iter().enumerate() {
            let (x, y) = if vertical {
                (0, i as i32 * 48)
            } else {
                (i as i32 * 48, 0)
            };
            assert_eq!(
                super::chart_at(&kinds, vertical, w, h, x, y).unwrap().0,
                kind
            );
        }
    }
    let editor = prefs(&panel);
    editor.enable_chart.emit_clicked();
    editor.move_up.emit_clicked();
    editor.dialog.response(gtk::ResponseType::Cancel);
    assert_eq!(
        panel.config.borrow().order,
        [
            ChartKind::QubeCpu,
            ChartKind::Ram,
            ChartKind::QubeRam,
            ChartKind::Network,
            ChartKind::Swap,
            ChartKind::Disk,
            ChartKind::Cpu
        ]
    );
    assert!(!panel.config.borrow().enabled[ChartKind::Cpu.index()]);
    let editor = prefs(&panel);
    for _ in 0..5 {
        editor.disable_chart.emit_clicked();
    }
    assert_eq!(editor.chart_list.children().len(), 1);
    assert!(!editor.disable_chart.is_sensitive());
    assert!(!editor.move_up.is_sensitive());
    assert!(!editor.move_down.is_sensitive());
    editor.disable_chart.emit_clicked();
    assert_eq!(editor.chart_list.children().len(), 1);
    editor.dialog.response(gtk::ResponseType::Cancel);
    panel.shutdown();
    println!("Chart lists: repeated moves, enable/disable, last-chart protection, Apply/Cancel and tooltips: passed");
}
// Keep all GTK checks in one test so they share the thread that initializes GTK.
#[test]
#[ignore = "needs a display and isolated settings; run make gui-test"]
fn preferences_and_rendering() {
    use gtk::ResponseType;
    gtk::init().expect("cannot initialize GTK for GUI checks");
    let gradient = Selection::Builtin(BuiltinTheme::LightGradient).id();
    let light = Selection::Builtin(BuiltinTheme::SolarizedLight).id();
    let tango = Selection::Builtin(BuiltinTheme::Tango).id();
    let draft = Selection::Draft { source: None }.id();
    chart_order_test();
    renderer_test();
    let panel = Panel::new(None, true);
    let prefs = prefs(&panel);
    assert_eq!(prefs.notebook.n_pages(), 2);
    prefs.select(&gradient);
    prefs.select(&draft);
    assert!(!prefs.editor.borrow().unsaved());
    prefs.select(&light); // No discard dialog for an untouched draft.
    assert_eq!(
        prefs.editor.borrow().palettes[ChartKind::Cpu.index()].colors[1],
        0xFDF6E3FF
    );
    prefs.select(&gradient);
    assert_eq!(
        prefs.editor.borrow().palettes[ChartKind::Cpu.index()].colors[2],
        0xFDF6E300
    );
    prefs.border.set_value(2.);
    assert!(prefs.editor.borrow().unsaved());
    prefs.border.set_value(0.);
    assert!(!prefs.editor.borrow().unsaved());
    prefs.select(&light); // Undoing edits removes the warning too.
    prefs.select(&gradient);
    prefs.border.set_value(2.);
    assert!(prefs.editor.borrow().unsaved());
    let keep = prompts(vec![message(
        "Your changes aren't saved. Discard them?",
        false,
    )]);
    prefs.theme.set_active_id(Some(&tango));
    assert_eq!(keep.get(), 1);
    assert!(prefs.editor.borrow().unsaved());
    assert_eq!(
        prefs.editor.borrow().palettes[ChartKind::Cpu.index()].border,
        2
    );
    // Live preview must not leak into the committed panel settings/autosave.
    assert_eq!(panel.config.borrow().appearance, Appearance::default());
    click(&prefs.save_as, vec![Prompt::Name("Cream / Ω")]);
    let id = prefs.editor.borrow().selection.source().unwrap().to_owned();
    assert!(matches!(
        prefs.editor.borrow().selection,
        Selection::Saved(_)
    ));
    prefs.select(&draft);
    assert!(!prefs.editor.borrow().unsaved());
    assert!(!prefs.save_changes.is_sensitive());
    prefs.select(&format!("saved:{id}"));
    prefs.border.set_value(3.);
    assert_eq!(
        prefs.store.borrow().find(&id).unwrap().palettes[ChartKind::Cpu.index()].border,
        2
    );
    click(
        &prefs.save_as,
        vec![
            Prompt::Name("Cream / Ω"),
            message("Overwrite theme ‘Cream / Ω’?", false),
        ],
    );
    assert!(prefs.editor.borrow().unsaved());
    assert_eq!(
        prefs.store.borrow().find(&id).unwrap().palettes[ChartKind::Cpu.index()].border,
        2
    );
    click(
        &prefs.save_as,
        vec![
            Prompt::Name("Cream / Ω"),
            message("Overwrite theme ‘Cream / Ω’?", true),
        ],
    );
    assert_eq!(prefs.store.borrow().themes.len(), 1);
    prefs.border.set_value(4.);
    click(&prefs.save_changes, vec![message("Overwrite theme", true)]);
    assert_eq!(
        prefs.store.borrow().find(&id).unwrap().palettes[ChartKind::Cpu.index()].border,
        4
    );
    click(&prefs.rename, vec![Prompt::Name("Renamed cream")]);
    assert!(prefs.store.borrow().by_name("Renamed cream").is_some());
    click(
        &prefs.save_as,
        vec![
            Prompt::Name("Original"),
            Prompt::Message("reserved", ResponseType::Close),
        ],
    );
    assert_eq!(prefs.store.borrow().themes.len(), 1);
    prefs.border.set_value(5.);
    let dir = prefs.store.borrow().dir.clone();
    let backup = dir.with_extension("backup");
    std::fs::rename(&dir, &backup).unwrap();
    std::fs::write(&dir, "blocked").unwrap();
    click(
        &prefs.save_changes,
        vec![
            message("Overwrite theme", true),
            Prompt::Message("Cannot create theme directory", ResponseType::Close),
        ],
    );
    assert!(prefs.editor.borrow().unsaved());
    assert_eq!(
        prefs.editor.borrow().palettes[ChartKind::Cpu.index()].border,
        5
    );
    assert_eq!(
        prefs.store.borrow().find(&id).unwrap().palettes[ChartKind::Cpu.index()].border,
        4
    );
    std::fs::remove_file(&dir).unwrap();
    std::fs::rename(backup, &dir).unwrap();
    click(&prefs.save_as, vec![Prompt::Name("Second")]);
    click(
        &prefs.delete,
        vec![message("Delete theme ‘Second’?", false)],
    );
    assert_eq!(prefs.store.borrow().themes.len(), 2);
    click(&prefs.delete, vec![message("Delete theme ‘Second’?", true)]);
    assert!(!prefs.editor.borrow().unsaved());
    assert_eq!(
        prefs.editor.borrow().palettes[ChartKind::Cpu.index()].border,
        5
    );
    prefs.theme.set_active_id(Some(&format!("saved:{id}")));
    assert_eq!(
        prefs.editor.borrow().palettes[ChartKind::Cpu.index()].border,
        4
    );
    prefs.dialog.response(ResponseType::Ok);
    let path = glib::user_config_dir().join("panel-roundtrip.rc");
    panel.config.borrow().save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap(), *panel.config.borrow());
    let prefs = self::prefs(&panel);
    assert!(matches!(
        prefs.editor.borrow().selection,
        Selection::Saved(_)
    ));
    prefs
        .chart
        .set_active(Some(ChartKind::QubeRam.index() as u32));
    prefs.transparent.set_active(true);
    emit_color(&prefs, 3, gtk::gdk::RGBA::new(0.2, 0.4, 0.6, 0.5));
    assert_eq!(
        prefs.editor.borrow().palettes[ChartKind::QubeRam.index()].colors[3],
        0x33669980
    );
    assert_eq!(
        prefs.editor.borrow().palettes[ChartKind::QubeRam.index()].colors[1] & 255,
        0
    );
    screenshot(&prefs);
    prefs.width.set_value(64.);
    prefs
        .chart_list
        .select_row(prefs.chart_list.row_at_index(0).as_ref());
    prefs.disable_chart.emit_clicked();
    prefs.dialog.response(ResponseType::Ok);
    panel.config.borrow().save(&path).unwrap();
    let loaded = Config::load(&path).unwrap();
    assert!(loaded.appearance.unsaved());
    assert_eq!(loaded.appearance.selection.source(), Some(id.as_str()));
    assert_eq!(loaded.width, 64);
    assert!(!loaded.enabled[ChartKind::Cpu.index()]);
    assert_eq!(
        ThemeStore::load().0.find(&id).unwrap().palettes[ChartKind::QubeRam.index()].colors[3],
        Appearance::builtin(BuiltinTheme::LightGradient).palettes[ChartKind::QubeRam.index()]
            .colors[3]
    );
    let committed = panel.config.borrow().clone();
    let prefs = self::prefs(&panel);
    let discard = prompts(vec![message(
        "Your changes aren't saved. Discard them?",
        true,
    )]);
    prefs.theme.set_active_id(Some(&tango));
    assert_eq!(discard.get(), 1);
    prefs.dialog.response(ResponseType::Cancel);
    assert_eq!(*panel.config.borrow(), committed);
    assert_eq!(panel.graphs.borrow().palette, committed.appearance.palettes);
    panel.vertical.set(true);
    panel.resize();
    let kinds = [
        ChartKind::Cpu,
        ChartKind::Ram,
        ChartKind::Network,
        ChartKind::Swap,
    ];
    assert_eq!(
        super::chart_at(&kinds, false, 130, 49, 32, 0).unwrap().0,
        ChartKind::Ram
    );
    assert!(super::chart_at(&kinds, false, 130, 49, 128, 0).is_none());
    assert_eq!(
        super::chart_at(&kinds, true, 49, 130, 0, 64).unwrap().0,
        ChartKind::Network
    );
    ThemeStore::load().0.delete(&id).unwrap();
    panel.shutdown();
    for action in 0..2 {
        let panel = Panel::new(None, false);
        let prefs = self::prefs(&panel);
        prefs.border.set_value(2.);
        let weak = Rc::downgrade(&panel);
        let owner = panel.clone();
        glib::idle_add_local_once(move || owner.shutdown());
        if action == 0 {
            prefs.theme.set_active_id(Some(&tango));
        } else {
            prefs.save_as.emit_clicked();
        }
        assert!(prefs.closed.get());
        drop(panel);
        assert!(weak.upgrade().is_none());
    }
    println!("Rust GTK: themes, discard/overwrite, errors, settings, tooltips, orientation and modal teardown: passed");
}
fn renderer_test() {
    for kind in ChartKind::ALL {
        let mut graphs = Graphs::new();
        graphs.palette[kind.index()].colors.fill(0);
        graphs.palette[kind.index()].colors[0] = 0xFF000080;
        graphs.palette[kind.index()].border = 3;
        let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 20, 20).unwrap();
        {
            let cr = cairo::Context::new(&surface).unwrap();
            super::draw_graphs(&graphs, &cr, 20, 20, &[kind], false);
        }
        surface.flush();
        let data = surface.data().unwrap();
        let pixel = |x: usize, y: usize| {
            u32::from_ne_bytes(
                data[(y * 20 + x) * 4..(y * 20 + x + 1) * 4]
                    .try_into()
                    .unwrap(),
            )
        };
        assert_eq!(pixel(0, 0), 0x80800000);
        assert_eq!(pixel(2, 2), 0x80800000);
        assert_eq!(pixel(3, 3), 0);
        assert_eq!(pixel(10, 10), 0);
    }
}
