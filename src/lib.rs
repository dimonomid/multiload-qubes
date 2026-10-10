//! Metrics and Cairo renderer. GTK/XFCE ownership stays in the small C adapter.
use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CString};
use std::fs;
use std::time::SystemTime;

#[derive(Clone, Debug)]
pub struct Sample {
    pub timestamp: f64,
    pub cpu: Option<f64>,
    pub total: u64,
    pub allocated: u64,
    pub used: u64,
    pub unknown: u64,
    pub overhead: u64,
    pub network: Option<(f64, f64)>,
}

impl Sample {
    // Versioned, bounded, numeric-only collector protocol; all memory is KiB.
    pub fn parse(text: &str) -> Option<Self> {
        if text.len() > 1024 {
            return None;
        }
        let p: Vec<_> = text.split_whitespace().collect();
        if !((p.len() == 9 && p[0] == "MQ1") || (p.len() == 11 && p[0] == "MQ2")) {
            return None;
        }
        let timestamp: f64 = p[1].parse().ok()?;
        let cpu = if p[2] == "unknown" {
            None
        } else {
            let v: f64 = p[2].parse().ok()?;
            if !v.is_finite() || !(0.0..=100.0).contains(&v) {
                return None;
            }
            Some(v)
        };
        let network = if p[0] == "MQ1" || (p[9] == "unknown" && p[10] == "unknown") {
            None
        } else {
            let rx: f64 = p[9].parse().ok()?;
            let tx: f64 = p[10].parse().ok()?;
            if !rx.is_finite()
                || !tx.is_finite()
                || !(0.0..=1e15).contains(&rx)
                || !(0.0..=1e15).contains(&tx)
            {
                return None;
            }
            Some((rx, tx))
        };
        let s = Self {
            timestamp,
            cpu,
            network,
            total: p[3].parse().ok()?,
            allocated: p[4].parse().ok()?,
            used: p[5].parse().ok()?,
            unknown: p[6].parse().ok()?,
            overhead: p[7].parse().ok()?,
        };
        let period: f64 = p[8].parse().ok()?;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs_f64();
        if !timestamp.is_finite()
            || !period.is_finite()
            || !(0.1..=60.0).contains(&period)
            || timestamp > now + 5.0
            || now - timestamp > (period * 4.0).max(5.0)
            || s.total == 0
            || s.allocated > s.total
            || s.used > s.allocated
            || s.unknown > s.allocated
            || s.used > s.allocated - s.unknown
            || s.overhead > s.total - s.allocated
        {
            return None;
        }
        Some(s)
    }
}

pub struct Graphs {
    history: VecDeque<Option<Sample>>,
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
            Some(Sample {
                timestamp: self.tick as f64,
                cpu: Some(35.0 + t.sin() * 25.0),
                total: 32 * 1048576,
                allocated: 24 * 1048576,
                used: ((10.0 + t.cos() * 3.0) * 1048576.0) as u64,
                unknown: 4 * 1048576,
                overhead: 256 * 1024,
                network: Some((
                    (1.0 + t.sin()) * 800000.0,
                    (1.0 + (t * 1.7).cos()) * 200000.0,
                )),
            })
        } else {
            fs::read_to_string("/run/multiload-qubes/metrics")
                .ok()
                .and_then(|s| Sample::parse(&s))
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
            format!("{}CPU: {}\nKnown used RAM: {:.2} GiB\nUnknown usage (allocated): {:.2} GiB\nKnown allocated spare: {:.2} GiB\nTotal allocated RAM: {:.2} GiB\nPhysical total: {:.2} GiB\nXen/unattributed overhead: {:.2} GiB\nRAM used = MemTotal − MemAvailable; swap excluded.{}",
                if demo { "DEMO — simulated data\n" } else { "" }, cpu, gib(s.used),
                gib(s.unknown), gib(s.allocated - s.unknown - s.used),
                gib(s.allocated), gib(s.total), gib(s.overhead), network)
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
    vertical: i32,
) {
    let Some(g) = g.as_ref() else {
        return;
    };
    if cr.is_null() || width < 4 || height < 4 {
        return;
    }
    let count = (cpu != 0) as i32 + (ram != 0) as i32 + (network != 0) as i32;
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
    for (enabled, kind) in [(cpu, 0), (ram, 1), (network, 2)] {
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
    fn line(cpu: &str, values: &str) -> String {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        format!("MQ1 {now} {cpu} {values} 1")
    }
    #[test]
    fn accepts_valid_snapshot() {
        assert!(Sample::parse(&line("42.5", "32000 24000 10000 2000 1000")).is_some());
    }
    #[test]
    fn network_protocol_and_validation() {
        let base = line("10", "32 24 10 0 1").replacen("MQ1", "MQ2", 1);
        let s = Sample::parse(&format!("{base} 1024 2048")).unwrap();
        assert_eq!(s.network, Some((1024.0, 2048.0)));
        assert!(Sample::parse(&format!("{base} unknown unknown"))
            .unwrap()
            .network
            .is_none());
        for values in ["NaN 0", "-1 0", "0 inf", "unknown 0", "1e16 0"] {
            assert!(Sample::parse(&format!("{base} {values}")).is_none());
        }
    }
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
    fn rejects_impossible_or_nonfinite_values() {
        for cpu in ["NaN", "inf", "-1", "101"] {
            assert!(Sample::parse(&line(cpu, "32 24 10 0 1")).is_none());
        }
        for values in [
            "32 33 10 0 0",
            "32 24 25 0 0",
            "32 24 10 20 0",
            "32 24 10 0 9",
        ] {
            assert!(Sample::parse(&line("10", values)).is_none());
        }
    }
    #[test]
    fn rejects_stale_data_and_accepts_cpu_warmup() {
        assert!(Sample::parse("MQ1 0 10 32 24 10 0 1 1").is_none());
        assert!(Sample::parse(&line("unknown", "32 24 10 0 1"))
            .unwrap()
            .cpu
            .is_none());
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
