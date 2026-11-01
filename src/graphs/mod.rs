//! Graph history, tooltips and typed Cairo rendering.
//!
//! Live samples follow metrics publications. Missing updates create gaps;
//! they must not look like measured zero usage.
mod domain_graph;
mod input;
use crate::palette::source;
use crate::palette::Palette;
use domain_graph::Metric;
pub(crate) use input::Version as InputVersion;

use std::borrow::Cow;
use std::collections::VecDeque;

use crate::Snapshot;

pub struct Graphs {
    history: VecDeque<Option<Snapshot>>,
    tooltips: [String; 7],
    tick: u64,
    pub(crate) palette: [Palette; 7],
}

impl Graphs {
    pub(crate) fn new() -> Self {
        Self {
            history: VecDeque::new(),
            tooltips: tooltips(None, false),
            tick: 0,
            palette: std::array::from_fn(|kind| {
                Palette::preset(crate::palette::BuiltinTheme::default(), kind)
            }),
        }
    }
    pub(crate) fn update(&mut self, demo: bool) {
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
            Self::read_snapshot(std::path::Path::new("/run/multiload-qubes/metrics"))
                .map(|(sample, _)| sample)
        };
        self.push_sample(sample, demo);
    }
    pub(crate) fn read_snapshot(path: &std::path::Path) -> Option<(Snapshot, InputVersion)> {
        let (text, version) = input::read_versioned(path).ok()?;
        Snapshot::decode_fresh(&text)
            .ok()
            .map(|snapshot| (snapshot, version))
    }
    pub(crate) fn push_sample(&mut self, sample: Option<Snapshot>, demo: bool) {
        self.tooltips = tooltips(sample.as_ref(), demo);
        self.history.push_back(sample);
        while self.history.len() > 2048 {
            self.history.pop_front();
        }
    }
}

fn tooltips(sample: Option<&Snapshot>, demo: bool) -> [String; 7] {
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
            "{name}: metrics unavailable or stale.\nCheck multiload-qubes-collector.service and multiload-qubes-parser.service in dom0."
        ))
    };
    text.map(|text| {
        format!(
            "{}{text}",
            if demo {
                "DEMO — simulated data\n"
            } else {
                ""
            }
        )
    })
}

fn rate_text(bytes: f64) -> String {
    if bytes >= 1048576.0 {
        format!("{:.2} MiB/s", bytes / 1048576.0)
    } else {
        format!("{:.1} KiB/s", bytes / 1024.0)
    }
}

fn rect(
    cr: &cairo::Context,
    color: u32,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<(), cairo::Error> {
    source(cr, color);
    cr.rectangle(x, y, w, h);
    cr.fill()
}

impl Graphs {
    fn io_scale(&self, columns: usize, kind: i32) -> f64 {
        // Keep small background transfers from filling the network chart.
        let minimum = if kind == 2 { 256.0 * 1024.0 } else { 1024.0 };
        // The two rates are stacked, so the scale must cover their sum. Only
        // visible history counts; an old peak stops affecting the scale once it scrolls away.
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
            .fold(minimum, f64::max);
        2f64.powf(peak.log2().ceil())
    }
    fn chart(
        &self,
        cr: &cairo::Context,
        width: i32,
        height: i32,
        kind: i32,
    ) -> Result<(), cairo::Error> {
        let palette = &self.palette[kind as usize];
        let border = palette
            .border
            .min(((width.min(height) - 2) / 2).max(0) as u32) as i32;
        let (w, h) = (width as f64, height as f64);
        let b = border as f64;
        rect(cr, palette.colors[0], 0.0, 0.0, w, b)?;
        rect(cr, palette.colors[0], 0.0, h - b, w, b)?;
        rect(cr, palette.colors[0], 0.0, b, b, h - 2.0 * b)?;
        rect(cr, palette.colors[0], w - b, b, b, h - 2.0 * b)?;
        // Use the existing one-pixel-inset plot coordinates with any border width.
        let width = width - 2 * border + 2;
        let height = height - 2 * border + 2;
        let (w, h) = (width as f64, height as f64);
        cr.save()?;
        cr.translate(b - 1.0, b - 1.0);
        cr.rectangle(1.0, 1.0, w - 2.0, h - 2.0);
        cr.clip();
        cr.set_antialias(cairo::Antialias::None);
        for y in 1..height - 1 {
            let t = if height > 3 {
                (y - 1) as f64 / (h - 3.0)
            } else {
                0.0
            };
            rect(
                cr,
                crate::palette::mix(palette.colors[1], palette.colors[2], t),
                1.0,
                y as f64,
                w - 2.0,
                1.0,
            )?;
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
            )?;
            cr.restore()?;
            return Ok(());
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
                        h - 1.0 - allocated,
                        1.0,
                        (allocated - unknown - used).max(0.0),
                    )?;
                    // Unknown usage sits above known usage, below confirmed spare RAM.
                    if unknown > 0.0 {
                        rect(
                            cr,
                            palette.colors[5],
                            x,
                            h - 1.0 - used - unknown,
                            1.0,
                            unknown,
                        )?;
                    }
                    rect(cr, palette.colors[3], x, h - 1.0 - used, 1.0, used)?;
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
                        rect(cr, read_color, x, h - 1.0 - incoming, 1.0, incoming)?;
                        rect(
                            cr,
                            write_color,
                            x,
                            h - 1.0 - incoming - outgoing,
                            1.0,
                            outgoing,
                        )?;
                    } else {
                        rect(cr, palette.colors[6], x, h - 3.0, 1.0, 2.0)?;
                    }
                } else if let Some(cpu) = s.cpu {
                    let v = cpu / 100.0 * (h - 2.0);
                    rect(cr, palette.colors[3], x, h - 1.0 - v, 1.0, v)?;
                }
            } else {
                rect(cr, palette.colors[6], x, h - 3.0, 1.0, 2.0)?;
            }
        }
        cr.restore()
    }
}

impl Graphs {
    pub(crate) fn tooltip(
        &self,
        kind: usize,
        chart_width: i32,
        chart_height: i32,
    ) -> Option<Cow<'_, str>> {
        let text = self.tooltips.get(kind)?;
        if matches!(kind, 2 | 4) && chart_width >= 4 {
            let border = self.palette[kind]
                .border
                .min(((chart_width.min(chart_height.max(4)) - 2) / 2) as u32);
            let scale = self.io_scale((chart_width - 2 * border as i32) as usize, kind as i32);
            let combined = if kind == 2 {
                "in + out"
            } else {
                "read + write"
            };
            Some(Cow::Owned(format!(
                "{text}\nScale: {} ({combined})",
                rate_text(scale)
            )))
        } else {
            Some(Cow::Borrowed(text))
        }
    }
    pub(crate) fn draw(
        &self,
        cr: &cairo::Context,
        width: i32,
        height: i32,
        kinds: &[usize],
        vertical: bool,
    ) -> Result<(), cairo::Error> {
        if width < 4 || height < 4 {
            return Ok(());
        }
        let count = kinds.len() as i32;
        if count == 0 {
            return Ok(());
        }
        let (cw, ch) = if vertical {
            (width, height / count)
        } else {
            (width / count, height)
        };
        if cw < 4 || ch < 4 {
            return Ok(());
        }
        cr.save()?;
        cr.rectangle(0., 0., width as f64, height as f64);
        cr.clip();
        for (index, &kind) in kinds.iter().enumerate() {
            if index > 0 {
                cr.translate(
                    if vertical { 0. } else { cw as f64 },
                    if vertical { ch as f64 } else { 0. },
                );
            }
            self.chart(cr, cw, ch, kind as i32)?;
        }
        cr.restore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ram_stacks_used_then_unknown_then_spare_without_overlapping_alpha() {
        let mut graphs = Graphs::new();
        graphs.update(true);
        let mut sample = graphs.history.pop_back().unwrap().unwrap();
        sample.total = 100;
        sample.allocated = 75;
        sample.used = 25;
        sample.unknown = 25;
        graphs.palette[1].colors = [0, 0, 0, 0x00FF0080, 0x0000FF80, 0xFF000080, 0];
        for unknown in [25, 0, 75] {
            sample.unknown = unknown;
            sample.used = if unknown == 75 { 0 } else { 25 };
            graphs.history.clear();
            graphs.history.push_back(Some(sample.clone()));
            let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 8, 100).unwrap();
            {
                let context = cairo::Context::new(&surface).unwrap();
                graphs.draw(&context, 8, 100, &[1], false).unwrap();
            }
            let stride = surface.stride() as usize;
            let pixels = surface.data().unwrap();
            let pixel = |y: usize| {
                let offset = y * stride + 7 * 4;
                u32::from_ne_bytes(pixels[offset..offset + 4].try_into().unwrap())
            };
            assert_eq!(pixel(10), 0); // Unallocated space remains transparent.
            if unknown == 75 {
                assert_eq!(pixel(35), 0x80800000);
                assert_eq!(pixel(85), 0x80800000);
            } else {
                assert_eq!(pixel(85), 0x80008000);
                assert_eq!(
                    pixel(60),
                    if unknown == 25 {
                        0x80800000
                    } else {
                        0x80000080
                    }
                );
                assert_eq!(pixel(35), 0x80000080);
            }
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
        assert_eq!(graphs.io_scale(2, 2), 256.0 * 1024.0);
        sample.network = Some((200.0 * 1024.0, 200.0 * 1024.0));
        graphs.history.push_back(Some(sample.clone()));
        assert_eq!(graphs.io_scale(2, 2), 512.0 * 1024.0);
        sample.network = Some((0.0, 0.0));
        graphs.history.push_back(Some(sample));
        assert_eq!(graphs.io_scale(1, 2), 256.0 * 1024.0);
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
            .as_str()
            .contains("warming up"));
        assert!(tooltips(Some(&sample), false)[4]
            .as_str()
            .contains("nvme0n1"));
        sample.disk.as_mut().unwrap().devices.clear();
        assert!(tooltips(Some(&sample), false)[4]
            .as_str()
            .contains("no supported physical disks"));
        sample.disk = None;
        assert!(tooltips(Some(&sample), false)[4]
            .as_str()
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
            let text = graphs.tooltips[kind].as_str();
            let body = text.strip_prefix("DEMO — simulated data\n").unwrap();
            assert!(body.starts_with(label));
            for (other, other_label) in expected.iter().enumerate() {
                if other != kind {
                    assert!(!body.lines().any(|line| line.starts_with(other_label)));
                }
            }
            assert!(text.contains("DEMO"));
            assert_eq!(graphs.tooltip(kind, 0, 0).as_deref(), Some(text));
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
            assert!(text.as_str().starts_with(label));
            assert!(text.as_str().contains("unavailable or stale"));
        }
        let mut sample = graphs.history.back().unwrap().clone().unwrap();
        sample.cpu = None;
        sample.network = None;
        sample.swap = None;
        let warming = tooltips(Some(&sample), false);
        assert!(warming[0].as_str().contains("warming up"));
        assert!(warming[2].as_str().contains("warming up"));
        assert!(warming[3].as_str().starts_with("Swap: unavailable"));
        assert!(graphs.tooltip(usize::MAX, 120, 48).is_none());
        assert!(graphs.tooltip(7, 120, 48).is_none());
    }

    #[test]
    fn tooltip_scale_matches_visible_columns_and_updates_after_resize() {
        fn text(g: &Graphs, kind: usize, width: i32) -> String {
            g.tooltip(kind, width, 48).unwrap().into_owned()
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
        assert!(text(&g, 2, 5).contains("Scale: 2.00 MiB/s (in + out)"));
        assert!(text(&g, 4, 5).contains("Scale: 4.00 MiB/s (read + write)"));
        // A narrower graph no longer includes the oldest peak. Query-time sizing
        // also covers vertical layouts and changes to which graphs are enabled.
        assert!(text(&g, 2, 4).contains("Scale: 256.0 KiB/s"));
        assert!(text(&g, 4, 4).contains("Scale: 4.0 KiB/s"));
        assert!(!text(&g, 1, 5).contains("Scale:"));
        // Border changes alter the visible history and therefore the scale.
        g.palette[2].border = 0;
        assert!(text(&g, 2, 4).contains("Scale: 2.00 MiB/s"));
        g.palette[2].border = 16;
        let scale = g.tooltip(2, 5, 4).unwrap();
        // Clamp the border using both dimensions, just as the renderer does.
        assert!(scale.contains("Scale: 2.00 MiB/s"));

        g.history.clear();
        assert!(text(&g, 2, 5).contains("Scale: 256.0 KiB/s"));
    }

    #[test]
    fn bounded_history_and_missing_data() {
        let mut g = Graphs::new();
        for _ in 0..3000 {
            g.update(true);
        }
        assert_eq!(g.history.len(), 2048);
        assert!(g.tooltips.iter().all(|t| t.as_str().contains("DEMO")));
    }
}
