//! Standalone demo and rendered chart images.
use super::charts::ChartKind;
use super::palette::BuiltinTheme;
use super::{config::Appearance, Graphs, Panel};
use clap::Parser;
use gtk::prelude::*;
use std::{path::PathBuf, process::ExitCode};

// Standalone preview options; this program does not connect to XFCE or the services.
#[derive(Parser)]
#[command(
    name = "multiload-qubes-preview",
    version,
    about = "Simulated Multiload for Qubes charts and Preferences"
)]
struct Args {
    // Render directly to a PNG instead of opening a GTK window.
    #[arg(long, value_name = "PNG", conflicts_with = "preferences")]
    render: Option<PathBuf>,
    // Image dimensions in pixels; each chart receives an equal share of the width.
    #[arg(long, requires = "render", value_parser = clap::value_parser!(i32).range(28..=16384))]
    render_width: Option<i32>,
    #[arg(long, requires = "render", value_parser = clap::value_parser!(i32).range(4..=4096))]
    render_height: Option<i32>,
    // Built-in numeric theme ID for reproducible rendered previews.
    #[arg(value_name = "THEME", requires = "render")]
    theme: Option<u32>,
    #[arg(long)]
    preferences: bool,
}

/// Render simulated charts or open a demo window; neither mode reads live host metrics.
pub fn run() -> ExitCode {
    let args = Args::parse();
    if let Some(path) = args.render {
        let theme = args
            .theme
            .and_then(BuiltinTheme::from_id)
            .unwrap_or_default();
        let (width, height) = (
            args.render_width.unwrap_or(840),
            args.render_height.unwrap_or(64),
        );
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height).unwrap();
        let context = cairo::Context::new(&surface).unwrap();
        let mut graphs = Graphs::new();
        graphs.set_history_width(width / ChartKind::COUNT as i32);
        graphs.palette = Appearance::builtin(theme).palettes;
        for _ in 0..400 {
            graphs.update(true);
        }
        super::draw_graphs(&graphs, &context, width, height, &ChartKind::ALL, false);
        return match std::fs::File::create(path)
            .map_err(|e| e.to_string())
            .and_then(|mut file| surface.write_to_png(&mut file).map_err(|e| e.to_string()))
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Cannot write preview: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if let Err(e) = gtk::init() {
        eprintln!("Cannot initialize GTK: {e}");
        return ExitCode::FAILURE;
    }
    let panel = Panel::new(None, true);
    panel.config.borrow_mut().width = 96;
    panel.resize();
    for _ in 0..400 {
        panel.tick();
    }
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Multiload for Qubes — DEMO");
    window.add(&panel.area);
    window.connect_destroy(|_| gtk::main_quit());
    window.show_all();
    if args.preferences {
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
    ExitCode::SUCCESS
}
