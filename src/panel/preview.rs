//! Standalone demo, renderer artifacts, and isolated GTK integration checks.
use super::{
    config::{Appearance, Config, Selection},
    preferences::Preferences,
    themes::ThemeStore,
    Panel,
};
use crate::palette::BuiltinTheme;
use gtk::prelude::*;
use std::{
    cell::Cell,
    ffi::{c_char, CStr},
    rc::Rc,
};

/// # Safety
/// `argv` contains `argc` valid NUL-terminated arguments, on the GTK thread.
#[no_mangle]
pub unsafe extern "C" fn mq_preview_main(argc: i32, argv: *const *const c_char) -> i32 {
    let args: Vec<String> = (0..argc)
        .map(|i| {
            CStr::from_ptr(*argv.add(i as usize))
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    if args.get(1).is_some_and(|s| s == "--render") {
        let Some(path) = args.get(2) else {
            return 1;
        };
        let theme = args
            .get(3)
            .and_then(|s| s.parse().ok())
            .and_then(BuiltinTheme::from_id)
            .unwrap_or(BuiltinTheme::Original);
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 840, 64).unwrap();
        let context = cairo::Context::new(&surface).unwrap();
        let mut graphs = crate::Graphs::new();
        graphs.palette = Appearance::builtin(theme).palettes;
        for _ in 0..400 {
            graphs.update(true);
        }
        super::draw_graphs(&graphs, &context, 840, 64, [true; 7], false);
        return match std::fs::File::create(path)
            .map_err(|e| e.to_string())
            .and_then(|mut file| surface.write_to_png(&mut file).map_err(|e| e.to_string()))
        {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Cannot write preview: {e}");
                1
            }
        };
    }
    if let Err(e) = gtk::init() {
        eprintln!("Cannot initialize GTK: {e}");
        return 1;
    }
    if args.get(1).is_some_and(|s| s == "--self-test") {
        self_test();
        return 0;
    }
    let panel = Panel::new(None, true);
    panel.config.borrow_mut().width = 96;
    for _ in 0..400 {
        panel.tick();
    }
    panel.resize();
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Multiload for Qubes — DEMO");
    window.add(&panel.area);
    window.connect_destroy(|_| gtk::main_quit());
    window.show_all();
    if args.iter().any(|s| s == "--preferences") {
        panel.configure();
    }
    if let Ok(seconds) = std::env::var("MQ_PREVIEW_SECONDS")
        .unwrap_or_default()
        .parse::<u32>()
    {
        let window = window.clone();
        glib::timeout_add_seconds_local_once(seconds, move || unsafe {
            window.destroy();
        });
    }
    gtk::main();
    panel.shutdown();
    0
}
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
fn self_test() {
    use gtk::ResponseType;
    renderer_test();
    let panel = Panel::new(None, true);
    let prefs = prefs(&panel);
    assert_eq!(prefs.notebook.n_pages(), 2);
    prefs.select("3");
    assert_eq!(prefs.editor.borrow().palettes[0].colors[1], 0xFDF6E3FF);
    prefs.select("4");
    assert_eq!(prefs.editor.borrow().palettes[0].colors[2], 0xFDF6E300);
    prefs.border.set_value(2.);
    assert!(prefs.editor.borrow().unsaved());
    let keep = prompts(vec![message(
        "Your changes aren't saved. Discard them?",
        false,
    )]);
    prefs.theme.set_active_id(Some("1"));
    assert_eq!(keep.get(), 1);
    assert!(prefs.editor.borrow().unsaved());
    assert_eq!(prefs.editor.borrow().palettes[0].border, 2);
    // Live preview must not leak into the committed panel settings/autosave.
    assert_eq!(panel.config.borrow().appearance, Appearance::default());
    click(&prefs.save_as, vec![Prompt::Name("Cream / Ω")]);
    let id = prefs.editor.borrow().selection.source().unwrap().to_owned();
    assert!(matches!(
        prefs.editor.borrow().selection,
        Selection::Saved(_)
    ));
    prefs.border.set_value(3.);
    assert_eq!(
        prefs.store.borrow().find(&id).unwrap().palettes[0].border,
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
        prefs.store.borrow().find(&id).unwrap().palettes[0].border,
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
        prefs.store.borrow().find(&id).unwrap().palettes[0].border,
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
    assert_eq!(prefs.editor.borrow().palettes[0].border, 5);
    assert_eq!(
        prefs.store.borrow().find(&id).unwrap().palettes[0].border,
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
    assert!(prefs.editor.borrow().unsaved());
    assert_eq!(prefs.editor.borrow().palettes[0].border, 5);
    let discard = prompts(vec![message(
        "Your changes aren't saved. Discard them?",
        true,
    )]);
    prefs.theme.set_active_id(Some(&format!("saved:{id}")));
    assert_eq!(discard.get(), 1);
    assert_eq!(prefs.editor.borrow().palettes[0].border, 4);
    prefs.dialog.response(ResponseType::Ok);
    let path = glib::user_config_dir().join("panel-roundtrip.rc");
    panel.config.borrow().save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap(), *panel.config.borrow());
    let prefs = super::preview::prefs(&panel);
    assert!(matches!(
        prefs.editor.borrow().selection,
        Selection::Saved(_)
    ));
    prefs.chart.set_active(Some(3));
    prefs.transparent.set_active(true);
    emit_color(&prefs, 3, gtk::gdk::RGBA::new(0.2, 0.4, 0.6, 0.5));
    assert_eq!(prefs.editor.borrow().palettes[5].colors[3], 0x33669980);
    assert_eq!(prefs.editor.borrow().palettes[5].colors[1] & 255, 0);
    screenshot(&prefs);
    prefs.width.set_value(64.);
    prefs.interval.set_value(500.);
    prefs.enabled[0].set_active(false);
    prefs.dialog.response(ResponseType::Ok);
    panel.config.borrow().save(&path).unwrap();
    let loaded = Config::load(&path).unwrap();
    assert!(loaded.appearance.unsaved());
    assert_eq!(loaded.appearance.selection.source(), Some(id.as_str()));
    assert_eq!(loaded.width, 64);
    assert_eq!(loaded.interval, 500);
    assert!(!loaded.enabled[0]);
    assert_eq!(
        ThemeStore::load().0.find(&id).unwrap().palettes[5].colors[3],
        Appearance::builtin(BuiltinTheme::SolarizedLightGradient).palettes[5].colors[3]
    );
    let committed = panel.config.borrow().clone();
    let prefs = super::preview::prefs(&panel);
    let discard = prompts(vec![message(
        "Your changes aren't saved. Discard them?",
        true,
    )]);
    prefs.theme.set_active_id(Some("1"));
    assert_eq!(discard.get(), 1);
    prefs.dialog.response(ResponseType::Cancel);
    assert_eq!(*panel.config.borrow(), committed);
    assert_eq!(panel.graphs.borrow().palette, committed.appearance.palettes);
    panel.vertical.set(true);
    panel.resize();
    let on = [true, false, true, false, true, true, false];
    assert_eq!(super::chart_at(on, false, 130, 49, 32, 0).unwrap().0, 1);
    assert!(super::chart_at(on, false, 130, 49, 128, 0).is_none());
    assert_eq!(super::chart_at(on, true, 49, 130, 0, 64).unwrap().0, 2);
    ThemeStore::load().0.delete(&id).unwrap();
    panel.shutdown();
    for action in 0..2 {
        let panel = Panel::new(None, false);
        let prefs = super::preview::prefs(&panel);
        prefs.border.set_value(2.);
        let weak = Rc::downgrade(&panel);
        let owner = panel.clone();
        glib::idle_add_local_once(move || owner.shutdown());
        if action == 0 {
            prefs.theme.set_active_id(Some("1"));
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
    for kind in 0..7 {
        let mut graphs = crate::Graphs::new();
        graphs.palette[kind].colors.fill(0);
        graphs.palette[kind].colors[0] = 0xFF000080;
        graphs.palette[kind].border = 3;
        let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 20, 20).unwrap();
        {
            let cr = cairo::Context::new(&surface).unwrap();
            super::draw_graphs(
                &graphs,
                &cr,
                20,
                20,
                super::config::CHART_KINDS.map(|k| k == kind),
                false,
            );
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
