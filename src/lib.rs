//! Metrics and Cairo renderer. GTK/XFCE ownership stays in the small C adapter.
mod swap;

use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CString};
use std::fs;

pub use multiload_qubes_snapshot::Snapshot;

pub struct Graphs {
    history: VecDeque<Option<Snapshot>>,
    tooltip: CString,
    tick: u64,
}

impl Graphs {
    fn new() -> Self {
        Self {
            history: VecDeque::new(),
            tooltip: CString::new("Waiting for collector").unwrap(),
            tick: 0,
        }
    }
    fn update(&mut self, demo: bool) {
        self.tick += 1;
        let sample = if demo {
            let t = self.tick as f64 / 9.0;
            Some(Snapshot {
                timestamp: self.tick as f64,
                cpu: Some(35.0 + t.sin() * 25.0),
                total: 32 * 1048576,
                allocated: 24 * 1048576,
                used: ((10.0 + t.cos() * 3.0) * 1048576.0) as u64,
                unknown: 4 * 1048576,
                overhead: 256 * 1024,
                period: 1.0,
                swap: Some(swap::demo(t)),
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
        let text = if let Some(s) = &sample {
            let cpu = s.cpu.map_or("warming up".into(), |v| format!("{v:.1}%"));
            let gib = |n: u64| n as f64 / 1048576.0;
            let network = match s.network {
                Some((rx, tx)) => format!(
                    "\nNetwork in: {}\nNetwork out: {}",
                    rate_text(rx),
                    rate_text(tx)
                ),
                None => "\nNetwork: unavailable or warming up".into(),
            };
            let swap = swap::tooltip(s.swap.as_ref());
            format!("{}CPU: {}\nKnown used RAM: {:.2} GiB\nUnknown usage (allocated): {:.2} GiB\nKnown allocated spare: {:.2} GiB\nTotal allocated RAM: {:.2} GiB\nPhysical total: {:.2} GiB\nXen/unattributed overhead: {:.2} GiB\nRAM used = MemTotal − MemAvailable; swap excluded.{}{}",
                if demo { "DEMO — simulated data\n" } else { "" }, cpu, gib(s.used),
                gib(s.unknown), gib(s.allocated - s.unknown - s.used),
                gib(s.allocated), gib(s.total), gib(s.overhead), network, swap)
        } else {
            "Metrics unavailable or stale.\nCheck multiload-qubes-collector.service in dom0.\nNo allocated memory is substituted for actual usage.".into()
        };
        self.tooltip = CString::new(text).unwrap();
        self.history.push_back(sample);
        while self.history.len() > 2048 {
            self.history.pop_front();
        }
    }
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
    fn cairo_set_line_width(cr: *mut c_void, width: f64);
    fn cairo_move_to(cr: *mut c_void, x: f64, y: f64);
    fn cairo_line_to(cr: *mut c_void, x: f64, y: f64);
    fn cairo_stroke(cr: *mut c_void);
    fn cairo_set_source_rgb(cr: *mut c_void, r: f64, g: f64, b: f64);
    fn cairo_rectangle(cr: *mut c_void, x: f64, y: f64, w: f64, h: f64);
    fn cairo_fill(cr: *mut c_void);
    fn cairo_save(cr: *mut c_void);
    fn cairo_restore(cr: *mut c_void);
    fn cairo_translate(cr: *mut c_void, x: f64, y: f64);
    fn cairo_clip(cr: *mut c_void);
}

unsafe fn rect(cr: *mut c_void, color: u32, x: f64, y: f64, w: f64, h: f64) {
    cairo_set_source_rgb(
        cr,
        ((color >> 16) & 255) as f64 / 255.0,
        ((color >> 8) & 255) as f64 / 255.0,
        (color & 255) as f64 / 255.0,
    );
    cairo_rectangle(cr, x, y, w, h);
    cairo_fill(cr);
}

impl Graphs {
    fn network_scale(&self, columns: usize) -> f64 {
        let peak = self
            .history
            .iter()
            .rev()
            .take(columns)
            .filter_map(|s| s.as_ref()?.network)
            .map(|(rx, tx)| rx + tx)
            .fold(1024.0, f64::max);
        2f64.powf(peak.log2().ceil())
    }
    unsafe fn chart(&self, cr: *mut c_void, width: i32, height: i32, kind: i32) {
        let (w, h) = (width as f64, height as f64);
        let (border, top) = match kind {
            1 => (0x008042, 0x13261D),
            2 => (0x807100, 0x262413),
            3 => (0x510080, 0x1F1326),
            _ => (0x005D80, 0x132126),
        };
        rect(cr, border, 0.0, 0.0, w, h);
        for y in 1..height - 1 {
            let scale = 1.0 - y as f64 / (h - 1.0);
            let c = (((((top >> 16) & 255) as f64 * scale) as u32) << 16)
                | (((((top >> 8) & 255) as f64 * scale) as u32) << 8)
                | (((top & 255) as f64 * scale) as u32);
            rect(cr, c, 1.0, y as f64, w - 2.0, 1.0);
        }
        if kind == 3 {
            self.swap_chart(cr, width, height);
            return;
        }
        let n = self.history.len().min((width - 2) as usize);
        let net_scale = self.network_scale(n);
        for (i, sample) in self.history.iter().skip(self.history.len() - n).enumerate() {
            let x = w - 1.0 - n as f64 + i as f64;
            if let Some(s) = sample {
                if kind == 1 {
                    let used = s.used as f64 / s.total as f64 * (h - 2.0);
                    let allocated = s.allocated as f64 / s.total as f64 * (h - 2.0);
                    let unknown = s.unknown as f64 / s.total as f64 * (h - 2.0);
                    rect(cr, 0x43D18D, x, h - 1.0 - allocated, 1.0, allocated);
                    // Unknown allocation is red, never represented as measured unused RAM.
                    if unknown > 0.0 {
                        rect(cr, 0xD14343, x, h - 1.0 - allocated, 1.0, unknown);
                    }
                    rect(cr, 0x03964F, x, h - 1.0 - used, 1.0, used);
                } else if kind == 2 {
                    if let Some((rx, tx)) = s.network {
                        let incoming = rx / net_scale * (h - 2.0);
                        let outgoing = tx / net_scale * (h - 2.0);
                        rect(cr, 0xE2CC05, x, h - 1.0 - incoming, 1.0, incoming);
                        rect(
                            cr,
                            0x696018,
                            x,
                            h - 1.0 - incoming - outgoing,
                            1.0,
                            outgoing,
                        );
                    } else {
                        rect(cr, 0x777777, x, h - 3.0, 1.0, 2.0);
                    }
                } else if let Some(cpu) = s.cpu {
                    let v = cpu / 100.0 * (h - 2.0);
                    rect(cr, 0x036F96, x, h - 1.0 - v, 1.0, v);
                }
            } else {
                rect(cr, 0x777777, x, h - 3.0, 1.0, 2.0);
            }
        }
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
/// `g` must be live; the returned string remains valid until the next tick/free.
#[no_mangle]
pub unsafe extern "C" fn mq_tooltip(g: *const Graphs) -> *const c_char {
    g.as_ref().map_or(std::ptr::null(), |g| g.tooltip.as_ptr())
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
    ram: i32,
    network: i32,
    swap: i32,
    vertical: i32,
) {
    let Some(g) = g.as_ref() else {
        return;
    };
    if cr.is_null() || width < 4 || height < 4 {
        return;
    }
    let count = (cpu != 0) as i32 + (ram != 0) as i32 + (network != 0) as i32 + (swap != 0) as i32;
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
    for (enabled, kind) in [(cpu, 0), (ram, 1), (network, 2), (swap, 3)] {
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
        assert_eq!(graphs.network_scale(2), 8192.0);
        sample.network = Some((0.0, 0.0));
        graphs.history.push_back(Some(sample));
        assert_eq!(graphs.network_scale(1), 1024.0);
    }
    #[test]
    fn bounded_history_and_missing_data() {
        let mut g = Graphs::new();
        for _ in 0..3000 {
            g.update(true);
        }
        assert_eq!(g.history.len(), 2048);
        assert!(g.tooltip.to_str().unwrap().contains("DEMO"));
    }
}
