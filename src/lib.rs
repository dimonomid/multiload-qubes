//! Metrics and Cairo renderer, with an optional Rust GTK3 panel runtime.
mod domain_graph;
mod palette;
#[cfg(feature = "panel")]
mod panel;
use domain_graph::Metric;
use palette::source;
use palette::Palette;

use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CString};
use std::fs;

pub use multiload_qubes_snapshot::Snapshot;

pub struct Graphs {
    history: VecDeque<Option<Snapshot>>,
    tooltips: [CString; 7],
    tooltip_view: CString,
    tick: u64,
    palette: [Palette; 7],
}

impl Graphs {
    fn new() -> Self {
        Self {
            history: VecDeque::new(),
            tooltips: tooltips(None, false),
            tooltip_view: CString::default(),
            tick: 0,
            palette: std::array::from_fn(|kind| {
                Palette::preset(palette::BuiltinTheme::Original, kind)
            }),
        }
    }
    fn update(&mut self, demo: bool) {
        self.tick += 1;
        let sample = if demo {
            let t = self.tick as f64 / 9.0;
            Some(Snapshot {
                timestamp: self.tick as f64,
                cpu: Some(35.0 + t.sin() * 25.0),
                qube_cpu: Some(domain_graph::demo_cpu(t * 0.9)),
                total: 32 * 1048576,
                allocated: 24 * 1048576,
                used: ((10.0 + t.cos() * 3.0) * 1048576.0) as u64,
                unknown: 4 * 1048576,
                overhead: 256 * 1024,
                period: 1.0,
                swap: Some(domain_graph::demo(t, Metric::Swap)),
                qube_ram: Some(domain_graph::demo(t * 0.7, Metric::Ram)),
                disk: Some(multiload_qubes_snapshot::DiskSnapshot {
                    devices: vec!["nvme0n1".into()],
                    rates: Some((
                        (1.0 + (t * 0.7).sin()) * 8_000_000.0,
                        (1.0 + (t * 1.3).cos()) * 3_000_000.0,
                    )),
                }),
                network: Some((
                    (1.0 + t.sin()) * 800000.0,
                    (1.0 + (t * 1.7).cos()) * 200000.0,
                )),
            })
        } else {
            fs::read_to_string("/run/multiload-qubes/metrics")
                .ok()
                .and_then(|s| Snapshot::decode_fresh(&s).ok())
        };
        // Repeat a still-fresh sample at the chart interval. Missing/stale data creates a gap.
        self.tooltips = tooltips(sample.as_ref(), demo);
        self.history.push_back(sample);
        while self.history.len() > 2048 {
            self.history.pop_front();
        }
    }
}

fn tooltips(sample: Option<&Snapshot>, demo: bool) -> [CString; 7] {
    let text = if let Some(s) = sample {
        let cpu = s
            .cpu
            .map_or("unavailable or warming up".into(), |v| format!("{v:.1}%"));
        let gib = |n: u64| n as f64 / 1048576.0;
        let ram = format!("Known used RAM: {:.2} GiB\nUnknown usage (allocated): {:.2} GiB\nKnown allocated spare: {:.2} GiB\nTotal allocated RAM: {:.2} GiB\nPhysical total: {:.2} GiB\nXen/unattributed overhead: {:.2} GiB\nRAM used = MemTotal − MemAvailable; swap excluded.",
            gib(s.used), gib(s.unknown), gib(s.allocated - s.unknown - s.used),
            gib(s.allocated), gib(s.total), gib(s.overhead));
        let network = match s.network {
            Some((rx, tx)) => format!(
                "Network in: {}\nNetwork out: {}",
                rate_text(rx),
                rate_text(tx)
            ),
            None => "Network: unavailable or warming up".into(),
        };
        let disk = match &s.disk {
            Some(disk) if disk.devices.is_empty() => {
                "Disk: no supported physical disks visible in dom0.".into()
            }
            Some(disk) => {
                let rates = disk.rates.map_or_else(
                    || "Disk: unavailable or warming up".into(),
                    |(read, write)| {
                        format!(
                            "Disk read: {}\nDisk write: {}",
                            rate_text(read),
                            rate_text(write)
                        )
                    },
                );
                format!(
                    "{rates}\nDevices: {}\nPhysical disk activity",
                    disk.devices.join(", ")
                )
            }
            None => "Disk: unavailable (collection failure or legacy collector)".into(),
        };
        [
            format!("CPU: {cpu}\n100% = all host logical CPUs busy."),
            ram,
            network,
            domain_graph::tooltip(s.swap.as_ref(), Metric::Swap),
            disk,
            domain_graph::tooltip(s.qube_ram.as_ref(), Metric::Ram),
            domain_graph::cpu_tooltip(s.qube_cpu.as_ref()),
        ]
    } else {
        ["CPU", "RAM", "Network", "Swap", "Disk", "Qube RAM", "Qube CPU"].map(|name| format!(
            "{name}: metrics unavailable or stale.\nCheck multiload-qubes-collector.service in dom0."
        ))
    };
    text.map(|text| {
        CString::new(format!(
            "{}{text}",
            if demo {
                "DEMO — simulated data\n"
            } else {
                ""
            }
        ))
        .unwrap()
    })
}

fn rate_text(bytes: f64) -> String {
    if bytes >= 1048576.0 {
        format!("{:.2} MiB/s", bytes / 1048576.0)
    } else {
        format!("{:.1} KiB/s", bytes / 1024.0)
    }
}

// Only these Cairo functions cross the FFI; the caller owns the draw context.
#[link(name = "cairo")]
extern "C" {
    fn cairo_set_source_rgba(cr: *mut c_void, r: f64, g: f64, b: f64, a: f64);
    fn cairo_set_antialias(cr: *mut c_void, antialias: i32);
    fn cairo_set_line_width(cr: *mut c_void, width: f64);
    fn cairo_move_to(cr: *mut c_void, x: f64, y: f64);
    fn cairo_line_to(cr: *mut c_void, x: f64, y: f64);
    fn cairo_stroke(cr: *mut c_void);
    fn cairo_rectangle(cr: *mut c_void, x: f64, y: f64, w: f64, h: f64);
    fn cairo_fill(cr: *mut c_void);
    fn cairo_save(cr: *mut c_void);
    fn cairo_restore(cr: *mut c_void);
    fn cairo_translate(cr: *mut c_void, x: f64, y: f64);
    fn cairo_clip(cr: *mut c_void);
}

unsafe fn rect(cr: *mut c_void, color: u32, x: f64, y: f64, w: f64, h: f64) {
    source(cr, color);
    cairo_rectangle(cr, x, y, w, h);
    cairo_fill(cr);
}

impl Graphs {
    fn io_scale(&self, columns: usize, kind: i32) -> f64 {
        let peak = self
            .history
            .iter()
            .rev()
            .take(columns)
            .filter_map(|s| {
                let s = s.as_ref()?;
                if kind == 4 {
                    s.disk.as_ref()?.rates
                } else {
                    s.network
                }
            })
            .map(|(rx, tx)| rx + tx)
            .fold(1024.0, f64::max);
        2f64.powf(peak.log2().ceil())
    }
    unsafe fn chart(&self, cr: *mut c_void, width: i32, height: i32, kind: i32) {
        let palette = &self.palette[kind as usize];
        let border = palette
            .border
            .min(((width.min(height) - 2) / 2).max(0) as u32) as i32;
        let (w, h) = (width as f64, height as f64);
        let b = border as f64;
        rect(cr, palette.colors[0], 0.0, 0.0, w, b);
        rect(cr, palette.colors[0], 0.0, h - b, w, b);
        rect(cr, palette.colors[0], 0.0, b, b, h - 2.0 * b);
        rect(cr, palette.colors[0], w - b, b, b, h - 2.0 * b);
        // Use the existing one-pixel-inset plot coordinates with any border width.
        let width = width - 2 * border + 2;
        let height = height - 2 * border + 2;
        let (w, h) = (width as f64, height as f64);
        cairo_save(cr);
        cairo_translate(cr, b - 1.0, b - 1.0);
        cairo_rectangle(cr, 1.0, 1.0, w - 2.0, h - 2.0);
        cairo_clip(cr);
        cairo_set_antialias(cr, 1);
        for y in 1..height - 1 {
            let t = if height > 3 {
                (y - 1) as f64 / (h - 3.0)
            } else {
                0.0
            };
            rect(
                cr,
                palette::mix(palette.colors[1], palette.colors[2], t),
                1.0,
                y as f64,
                w - 2.0,
                1.0,
            );
        }
        if matches!(kind, 3 | 5 | 6) {
            self.domain_chart(
                cr,
                width,
                height,
                palette,
                match kind {
                    6 => Metric::Cpu,
                    5 => Metric::Ram,
                    _ => Metric::Swap,
                },
            );
            cairo_restore(cr);
            return;
        }
        let n = self.history.len().min((width - 2) as usize);
        let io_scale = self.io_scale(n, kind);
        for (i, sample) in self.history.iter().skip(self.history.len() - n).enumerate() {
            let x = w - 1.0 - n as f64 + i as f64;
            if let Some(s) = sample {
                if kind == 1 {
                    let used = s.used as f64 / s.total as f64 * (h - 2.0);
                    let allocated = s.allocated as f64 / s.total as f64 * (h - 2.0);
                    let unknown = s.unknown as f64 / s.total as f64 * (h - 2.0);
                    // Draw disjoint stacks so translucent colors do not overlap.
                    rect(
                        cr,
                        palette.colors[4],
                        x,
                        h - 1.0 - allocated + unknown,
                        1.0,
                        (allocated - unknown - used).max(0.0),
                    );
                    // Unknown allocation is red, never represented as measured unused RAM.
                    if unknown > 0.0 {
                        rect(cr, palette.colors[5], x, h - 1.0 - allocated, 1.0, unknown);
                    }
                    rect(cr, palette.colors[3], x, h - 1.0 - used, 1.0, used);
                } else if kind == 2 || kind == 4 {
                    let (rates, read_color, write_color) = if kind == 4 {
                        (
                            s.disk.as_ref().and_then(|d| d.rates),
                            palette.colors[3],
                            palette.colors[4],
                        )
                    } else {
                        (s.network, palette.colors[3], palette.colors[4])
                    };
                    if let Some((rx, tx)) = rates {
                        let incoming = rx / io_scale * (h - 2.0);
                        let outgoing = tx / io_scale * (h - 2.0);
                        rect(cr, read_color, x, h - 1.0 - incoming, 1.0, incoming);
                        rect(
                            cr,
                            write_color,
                            x,
                            h - 1.0 - incoming - outgoing,
                            1.0,
                            outgoing,
                        );
                    } else {
                        rect(cr, palette.colors[6], x, h - 3.0, 1.0, 2.0);
                    }
                } else if let Some(cpu) = s.cpu {
                    let v = cpu / 100.0 * (h - 2.0);
                    rect(cr, palette.colors[3], x, h - 1.0 - v, 1.0, v);
                }
            } else {
                rect(cr, palette.colors[6], x, h - 3.0, 1.0, 2.0);
            }
        }
        cairo_restore(cr);
    }
}

/// # Safety
/// Returned handle must be freed exactly once with mq_free, on the GTK thread.
#[no_mangle]
pub extern "C" fn mq_new() -> *mut Graphs {
    Box::into_raw(Box::new(Graphs::new()))
}
/// # Safety
/// `g` must be a live handle from mq_new, exclusively owned by this thread.
#[no_mangle]
pub unsafe extern "C" fn mq_free(g: *mut Graphs) {
    if !g.is_null() {
        drop(Box::from_raw(g));
    }
}
/// # Safety
/// `g` must be a live, exclusively borrowed handle.
#[no_mangle]
pub unsafe extern "C" fn mq_tick(g: *mut Graphs, demo: i32) {
    if let Some(g) = g.as_mut() {
        g.update(demo != 0);
    }
}
/// # Safety
/// `g` must be live and exclusively borrowed on the GTK thread. The returned
/// string remains valid until the next tooltip query, tick or free.
#[no_mangle]
pub unsafe extern "C" fn mq_tooltip(
    g: *mut Graphs,
    kind: i32,
    chart_width: i32,
    chart_height: i32,
) -> *const c_char {
    let Some(g) = g.as_mut() else {
        return std::ptr::null();
    };
    let Some(text) = g.tooltips.get(kind as usize) else {
        return std::ptr::null();
    };
    if matches!(kind, 2 | 4) && chart_width >= 4 {
        let border = g.palette[kind as usize]
            .border
            .min(((chart_width.min(chart_height.max(4)) - 2) / 2) as u32);
        let scale = g.io_scale((chart_width - 2 * border as i32) as usize, kind);
        let combined = if kind == 2 {
            "in + out"
        } else {
            "read + write"
        };
        g.tooltip_view = CString::new(format!(
            "{}\nScale: {} ({combined})",
            text.to_str().unwrap(),
            rate_text(scale)
        ))
        .unwrap();
        g.tooltip_view.as_ptr()
    } else {
        text.as_ptr()
    }
}
/// # Safety
/// `g` and `cr` must be live on the GTK thread. Cairo context is borrowed only.
#[no_mangle]
#[allow(clippy::too_many_arguments)] // Fixed C ABI for the panel adapter.
pub unsafe extern "C" fn mq_draw(
    g: *const Graphs,
    cr: *mut c_void,
    width: i32,
    height: i32,
    cpu: i32,
    qube_cpu: i32,
    ram: i32,
    qube_ram: i32,
    network: i32,
    swap: i32,
    disk: i32,
    vertical: i32,
) {
    let Some(g) = g.as_ref() else {
        return;
    };
    if cr.is_null() || width < 4 || height < 4 {
        return;
    }
    let count = (cpu != 0) as i32
        + (qube_cpu != 0) as i32
        + (ram != 0) as i32
        + (qube_ram != 0) as i32
        + (network != 0) as i32
        + (swap != 0) as i32
        + (disk != 0) as i32;
    if count == 0 {
        return;
    }
    let (cw, ch) = if vertical != 0 {
        (width, height / count)
    } else {
        (width / count, height)
    };
    if cw < 4 || ch < 4 {
        return;
    }
    cairo_save(cr);
    cairo_rectangle(cr, 0.0, 0.0, width as f64, height as f64);
    cairo_clip(cr);
    let mut index = 0;
    for (enabled, kind) in [
        (cpu, 0),
        (qube_cpu, 6),
        (ram, 1),
        (qube_ram, 5),
        (network, 2),
        (swap, 3),
        (disk, 4),
    ] {
        if enabled == 0 {
            continue;
        }
        if index > 0 {
            cairo_translate(
                cr,
                if vertical == 0 { cw as f64 } else { 0.0 },
                if vertical != 0 { ch as f64 } else { 0.0 },
            );
        }
        g.chart(cr, cw, ch, kind);
        index += 1;
    }
    cairo_restore(cr);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_scale_covers_combined_rates_and_expires_old_peaks() {
        let mut graphs = Graphs::new();
        graphs.update(true);
        let mut sample = graphs.history.back().unwrap().clone().unwrap();
        graphs.history.clear();
        sample.network = Some((4096.0, 4096.0));
        graphs.history.push_back(Some(sample.clone()));
        assert_eq!(graphs.io_scale(2, 2), 8192.0);
        sample.network = Some((0.0, 0.0));
        graphs.history.push_back(Some(sample));
        assert_eq!(graphs.io_scale(1, 2), 1024.0);
    }
    #[test]
    fn disk_scale_covers_combined_rates_and_expires_old_peaks() {
        let mut g = Graphs::new();
        g.update(true);
        let mut sample = g.history.back().unwrap().clone().unwrap();
        g.history.clear();
        sample.disk.as_mut().unwrap().rates = Some((4096.0, 4096.0));
        g.history.push_back(Some(sample.clone()));
        assert_eq!(g.io_scale(2, 4), 8192.0);
        sample.disk.as_mut().unwrap().rates = Some((0.0, 0.0));
        g.history.push_back(Some(sample.clone()));
        assert_eq!(g.io_scale(1, 4), 1024.0);
        sample.disk.as_mut().unwrap().rates = None;
        assert!(tooltips(Some(&sample), false)[4]
            .to_str()
            .unwrap()
            .contains("warming up"));
        assert!(tooltips(Some(&sample), false)[4]
            .to_str()
            .unwrap()
            .contains("nvme0n1"));
        sample.disk.as_mut().unwrap().devices.clear();
        assert!(tooltips(Some(&sample), false)[4]
            .to_str()
            .unwrap()
            .contains("no supported physical disks"));
        sample.disk = None;
        assert!(tooltips(Some(&sample), false)[4]
            .to_str()
            .unwrap()
            .starts_with("Disk: unavailable"));
    }

    #[test]
    fn tooltips_only_include_the_selected_chart_and_handle_missing_metrics() {
        let mut graphs = Graphs::new();
        graphs.update(true);
        let expected = [
            "CPU:",
            "Known used RAM:",
            "Network in:",
            "Swap:",
            "Disk read:",
            "Qube RAM:",
            "Qube CPU:",
        ];
        for (kind, label) in expected.iter().enumerate() {
            let text = graphs.tooltips[kind].to_str().unwrap();
            let body = text.strip_prefix("DEMO — simulated data\n").unwrap();
            assert!(body.starts_with(label));
            for (other, other_label) in expected.iter().enumerate() {
                if other != kind {
                    assert!(!body.lines().any(|line| line.starts_with(other_label)));
                }
            }
            assert!(text.contains("DEMO"));
            // Verify that the native adapter's kind index selects the same string.
            assert_eq!(
                unsafe { mq_tooltip(&mut graphs, kind as i32, 0, 0) },
                graphs.tooltips[kind].as_ptr()
            );
        }
        let missing = tooltips(None, false);
        for (text, label) in missing.iter().zip([
            "CPU:",
            "RAM:",
            "Network:",
            "Swap:",
            "Disk:",
            "Qube RAM:",
            "Qube CPU:",
        ]) {
            assert!(text.to_str().unwrap().starts_with(label));
            assert!(text.to_str().unwrap().contains("unavailable or stale"));
        }
        let mut sample = graphs.history.back().unwrap().clone().unwrap();
        sample.cpu = None;
        sample.network = None;
        sample.swap = None;
        let warming = tooltips(Some(&sample), false);
        assert!(warming[0].to_str().unwrap().contains("warming up"));
        assert!(warming[2].to_str().unwrap().contains("warming up"));
        assert!(warming[3]
            .to_str()
            .unwrap()
            .starts_with("Swap: unavailable"));
        assert!(unsafe { mq_tooltip(&mut graphs, -1, 120, 48) }.is_null());
        assert!(unsafe { mq_tooltip(&mut graphs, 7, 120, 48) }.is_null());
    }

    #[test]
    fn tooltip_scale_matches_visible_columns_and_updates_after_resize() {
        fn text(g: &mut Graphs, kind: i32, width: i32) -> String {
            unsafe { std::ffi::CStr::from_ptr(mq_tooltip(g, kind, width, 48)) }
                .to_str()
                .unwrap()
                .into()
        }
        let mut g = Graphs::new();
        g.palette[2].border = 1;
        g.palette[4].border = 1;
        g.update(true);
        let mut sample = g.history.back().unwrap().clone().unwrap();
        g.history.clear();
        sample.network = Some((1048576.0, 1048576.0));
        sample.disk.as_mut().unwrap().rates = Some((2097152.0, 2097152.0));
        g.history.push_back(Some(sample.clone()));
        sample.network = Some((1024.0, 1024.0));
        sample.disk.as_mut().unwrap().rates = Some((2048.0, 2048.0));
        g.history.push_back(Some(sample.clone()));
        g.history.push_back(Some(sample));
        assert!(text(&mut g, 2, 5).contains("Scale: 2.00 MiB/s (in + out)"));
        assert!(text(&mut g, 4, 5).contains("Scale: 4.00 MiB/s (read + write)"));
        // A narrower graph no longer includes the oldest peak. Query-time sizing
        // also covers vertical layouts and changes to which graphs are enabled.
        assert!(text(&mut g, 2, 4).contains("Scale: 2.0 KiB/s"));
        assert!(text(&mut g, 4, 4).contains("Scale: 4.0 KiB/s"));
        assert!(!text(&mut g, 1, 5).contains("Scale:"));
        // Border changes alter the visible history and therefore the scale.
        g.palette[2].border = 0;
        assert!(text(&mut g, 2, 4).contains("Scale: 2.00 MiB/s"));
        g.palette[2].border = 16;
        let scale = unsafe { std::ffi::CStr::from_ptr(mq_tooltip(&mut g, 2, 5, 4)) };
        // Clamp the border using both dimensions, just as the renderer does.
        assert!(scale.to_str().unwrap().contains("Scale: 2.00 MiB/s"));

        g.history.clear();
        assert!(text(&mut g, 2, 5).contains("Scale: 1.0 KiB/s"));
    }

    #[test]
    fn bounded_history_and_missing_data() {
        let mut g = Graphs::new();
        for _ in 0..3000 {
            g.update(true);
        }
        assert_eq!(g.history.len(), 2048);
        assert!(g
            .tooltips
            .iter()
            .all(|t| t.to_str().unwrap().contains("DEMO")));
    }
}
