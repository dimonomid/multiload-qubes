//! Graph history, tooltips and typed Cairo rendering.
//!
//! Live samples follow metrics publications. Missing updates create gaps;
//! they must not look like measured zero usage.
mod domain_graph;
mod input;
use super::charts::ChartKind;
use super::palette::source;
use super::palette::Palette;
use domain_graph::Metric;
pub(crate) use input::Version as InputVersion;

use std::borrow::Cow;
use std::collections::VecDeque;

use crate::Snapshot;

pub struct Graphs {
    history: VecDeque<Option<Snapshot>>,
    history_width: usize,
    tooltips: [String; ChartKind::COUNT],
    tick: u64,
    pub(crate) palette: [Palette; ChartKind::COUNT],
}

impl Graphs {
    pub(crate) fn new() -> Self {
        Self {
            history: VecDeque::new(),
            history_width: 1,
            tooltips: tooltips(None, false),
            tick: 0,
            palette: std::array::from_fn(|kind| {
                Palette::preset(
                    super::palette::BuiltinTheme::default(),
                    ChartKind::ALL[kind],
                )
            }),
        }
    }
    pub(crate) fn set_history_width(&mut self, width: i32) {
        let width = width.max(1) as usize;
        if width == self.history_width {
            return;
        }
        self.history_width = width;
        while self.history.len() > width {
            self.history.pop_front();
        }
        // Drop old samples and release their deque slots when charts get narrower.
        if self.history.capacity() > width {
            self.history.shrink_to_fit();
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
        while self.history.len() >= self.history_width {
            self.history.pop_front();
        }
        self.history.push_back(sample);
    }
}

fn tooltips(sample: Option<&Snapshot>, demo: bool) -> [String; ChartKind::COUNT] {
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
            None => "Disk: unavailable".into(),
        };
        ChartKind::ALL.map(|kind| match kind {
            ChartKind::Cpu => format!("CPU: {cpu}\n100% = all host logical CPUs busy."),
            ChartKind::Ram => ram.clone(),
            ChartKind::Network => network.clone(),
            ChartKind::Swap => domain_graph::tooltip(s.swap.as_ref(), Metric::Swap),
            ChartKind::Disk => disk.clone(),
            ChartKind::QubeRam => domain_graph::tooltip(s.qube_ram.as_ref(), Metric::Ram),
            ChartKind::QubeCpu => domain_graph::cpu_tooltip(s.qube_cpu.as_ref()),
        })
    } else {
        ChartKind::ALL.map(|kind| format!(
            "{}: metrics unavailable or stale.\nCheck multiload-qubes-collector.service and multiload-qubes-parser.service in dom0.", kind.metadata().name
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
    fn io_scale(&self, columns: usize, kind: ChartKind) -> f64 {
        // Keep small background transfers from filling the network chart.
        let minimum = if kind == ChartKind::Network {
            256.0 * 1024.0
        } else {
            1024.0
        };
        // The two rates are stacked, so the scale must cover their sum. Only
        // visible history counts; an old peak stops affecting the scale once it scrolls away.
        let peak = self
            .history
            .iter()
            .rev()
            .take(columns)
            .filter_map(|s| {
                let s = s.as_ref()?;
                if kind == ChartKind::Disk {
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
        kind: ChartKind,
    ) -> Result<(), cairo::Error> {
        let palette = &self.palette[kind.index()];
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
                super::palette::mix(palette.colors[1], palette.colors[2], t),
                1.0,
                y as f64,
                w - 2.0,
                1.0,
            )?;
        }
        let domain_metric = match kind {
            ChartKind::QubeCpu => Some(Metric::Cpu),
            ChartKind::QubeRam => Some(Metric::Ram),
            ChartKind::Swap => Some(Metric::Swap),
            ChartKind::Cpu | ChartKind::Ram | ChartKind::Network | ChartKind::Disk => None,
        };
        if let Some(metric) = domain_metric {
            self.domain_chart(cr, width, height, palette, metric)?;
            cr.restore()?;
            return Ok(());
        }
        let n = self.history.len().min((width - 2) as usize);
        let io_scale = self.io_scale(n, kind);
        for (i, sample) in self.history.iter().skip(self.history.len() - n).enumerate() {
            let x = w - 1.0 - n as f64 + i as f64;
            if let Some(s) = sample {
                if kind == ChartKind::Ram {
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
                } else if kind.is_io() {
                    let (rates, read_color, write_color) = if kind == ChartKind::Disk {
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
                } else {
                    rect(cr, palette.colors[6], x, h - 3.0, 1.0, 2.0)?;
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
        kind: ChartKind,
        chart_width: i32,
        chart_height: i32,
    ) -> Cow<'_, str> {
        let text = &self.tooltips[kind.index()];
        if kind.is_io() && chart_width >= 4 {
            let border = self.palette[kind.index()]
                .border
                .min(((chart_width.min(chart_height.max(4)) - 2) / 2) as u32);
            let scale = self.io_scale((chart_width - 2 * border as i32) as usize, kind);
            let combined = if kind == ChartKind::Network {
                "in + out"
            } else {
                "read + write"
            };
            Cow::Owned(format!("{text}\nScale: {} ({combined})", rate_text(scale)))
        } else {
            Cow::Borrowed(text)
        }
    }
    pub(crate) fn draw(
        &self,
        cr: &cairo::Context,
        width: i32,
        height: i32,
        kinds: &[ChartKind],
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
            self.chart(cr, cw, ch, kind)?;
        }
        cr.restore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_cpu_uses_missing_marker_but_measured_zero_does_not() {
        let mut graphs = Graphs::new();
        graphs.set_history_width(3);
        graphs.update(true);
        let mut sample = graphs.history.pop_back().unwrap().unwrap();
        let palette = &mut graphs.palette[ChartKind::Cpu.index()];
        palette.colors.fill(0);
        palette.colors[6] = 0xFF0000FF;
        sample.cpu = Some(0.0);
        graphs.push_sample(Some(sample.clone()), false);
        sample.cpu = None;
        graphs.push_sample(Some(sample), false);
        graphs.push_sample(None, false);
        let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 8, 8).unwrap();
        {
            let cr = cairo::Context::new(&surface).unwrap();
            graphs.draw(&cr, 8, 8, &[ChartKind::Cpu], false).unwrap();
        }
        let stride = surface.stride() as usize;
        let pixels = surface.data().unwrap();
        let pixel = |x: usize, y: usize| {
            let offset = y * stride + x * 4;
            u32::from_ne_bytes(pixels[offset..offset + 4].try_into().unwrap())
        };
        // Left to right: measured idle, unknown CPU, and a missing snapshot.
        for y in 0..8 {
            assert_eq!(pixel(5, y), 0);
            let marker = if y >= 6 { 0xFFFF0000 } else { 0 };
            assert_eq!(pixel(6, y), marker);
            assert_eq!(pixel(7, y), marker);
        }
    }

    #[test]
    fn ram_stacks_used_then_unknown_then_spare_without_overlapping_alpha() {
        let mut graphs = Graphs::new();
        graphs.update(true);
        let mut sample = graphs.history.pop_back().unwrap().unwrap();
        sample.total = 100;
        sample.allocated = 75;
        sample.used = 25;
        sample.unknown = 25;
        graphs.palette[ChartKind::Ram.index()].colors =
            [0, 0, 0, 0x00FF0080, 0x0000FF80, 0xFF000080, 0];
        for unknown in [25, 0, 75] {
            sample.unknown = unknown;
            sample.used = if unknown == 75 { 0 } else { 25 };
            graphs.history.clear();
            graphs.history.push_back(Some(sample.clone()));
            let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 8, 100).unwrap();
            {
                let context = cairo::Context::new(&surface).unwrap();
                graphs
                    .draw(&context, 8, 100, &[ChartKind::Ram], false)
                    .unwrap();
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
        assert_eq!(graphs.io_scale(2, ChartKind::Network), 256.0 * 1024.0);
        sample.network = Some((200.0 * 1024.0, 200.0 * 1024.0));
        graphs.history.push_back(Some(sample.clone()));
        assert_eq!(graphs.io_scale(2, ChartKind::Network), 512.0 * 1024.0);
        sample.network = Some((0.0, 0.0));
        graphs.history.push_back(Some(sample));
        assert_eq!(graphs.io_scale(1, ChartKind::Network), 256.0 * 1024.0);
    }
    #[test]
    fn disk_scale_covers_combined_rates_and_expires_old_peaks() {
        let mut g = Graphs::new();
        g.update(true);
        let mut sample = g.history.back().unwrap().clone().unwrap();
        g.history.clear();
        sample.disk.as_mut().unwrap().rates = Some((4096.0, 4096.0));
        g.history.push_back(Some(sample.clone()));
        assert_eq!(g.io_scale(2, ChartKind::Disk), 8192.0);
        sample.disk.as_mut().unwrap().rates = Some((0.0, 0.0));
        g.history.push_back(Some(sample.clone()));
        assert_eq!(g.io_scale(1, ChartKind::Disk), 1024.0);
        sample.disk.as_mut().unwrap().rates = None;
        assert!(tooltips(Some(&sample), false)[ChartKind::Disk.index()]
            .as_str()
            .contains("warming up"));
        assert!(tooltips(Some(&sample), false)[ChartKind::Disk.index()]
            .as_str()
            .contains("nvme0n1"));
        sample.disk.as_mut().unwrap().devices.clear();
        assert!(tooltips(Some(&sample), false)[ChartKind::Disk.index()]
            .as_str()
            .contains("no supported physical disks"));
        sample.disk = None;
        assert!(tooltips(Some(&sample), false)[ChartKind::Disk.index()]
            .as_str()
            .starts_with("Disk: unavailable"));
    }

    #[test]
    fn tooltips_only_include_the_selected_chart_and_handle_missing_metrics() {
        let mut graphs = Graphs::new();
        graphs.update(true);
        let expected = [
            (ChartKind::Cpu, "CPU:"),
            (ChartKind::Ram, "Known used RAM:"),
            (ChartKind::Network, "Network in:"),
            (ChartKind::Swap, "Swap:"),
            (ChartKind::Disk, "Disk read:"),
            (ChartKind::QubeRam, "Qube RAM:"),
            (ChartKind::QubeCpu, "Qube CPU:"),
        ];
        for (kind, label) in expected {
            let text = graphs.tooltips[kind.index()].as_str();
            let body = text.strip_prefix("DEMO — simulated data\n").unwrap();
            assert!(body.starts_with(label));
            for (other, other_label) in expected {
                if other != kind {
                    assert!(!body.lines().any(|line| line.starts_with(other_label)));
                }
            }
            assert!(text.contains("DEMO"));
            assert_eq!(graphs.tooltip(kind, 0, 0).as_ref(), text);
        }
        let missing = tooltips(None, false);
        for (text, label) in missing.iter().zip([
            "CPU:",
            "Per-qube CPU:",
            "RAM:",
            "Per-qube RAM:",
            "Network:",
            "Per-qube swap:",
            "Disk I/O:",
        ]) {
            assert!(text.as_str().starts_with(label));
            assert!(text.as_str().contains("unavailable or stale"));
        }
        let mut sample = graphs.history.back().unwrap().clone().unwrap();
        sample.cpu = None;
        sample.network = None;
        sample.swap = None;
        let warming = tooltips(Some(&sample), false);
        assert!(warming[ChartKind::Cpu.index()]
            .as_str()
            .contains("warming up"));
        assert!(warming[ChartKind::Network.index()]
            .as_str()
            .contains("warming up"));
        assert!(warming[ChartKind::Swap.index()]
            .as_str()
            .starts_with("Swap: unavailable"));
    }

    #[test]
    fn tooltip_scale_matches_visible_columns_and_updates_after_resize() {
        fn text(g: &Graphs, kind: ChartKind, width: i32) -> String {
            g.tooltip(kind, width, 48).into_owned()
        }
        let mut g = Graphs::new();
        g.palette[ChartKind::Network.index()].border = 1;
        g.palette[ChartKind::Disk.index()].border = 1;
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
        assert!(text(&g, ChartKind::Network, 5).contains("Scale: 2.00 MiB/s (in + out)"));
        assert!(text(&g, ChartKind::Disk, 5).contains("Scale: 4.00 MiB/s (read + write)"));
        // A narrower graph no longer includes the oldest peak. Query-time sizing
        // also covers vertical layouts and changes to which graphs are enabled.
        assert!(text(&g, ChartKind::Network, 4).contains("Scale: 256.0 KiB/s"));
        assert!(text(&g, ChartKind::Disk, 4).contains("Scale: 4.0 KiB/s"));
        assert!(!text(&g, ChartKind::Ram, 5).contains("Scale:"));
        // Border changes alter the visible history and therefore the scale.
        g.palette[ChartKind::Network.index()].border = 0;
        assert!(text(&g, ChartKind::Network, 4).contains("Scale: 2.00 MiB/s"));
        g.palette[ChartKind::Network.index()].border = 16;
        let scale = g.tooltip(ChartKind::Network, 5, 4);
        // Clamp the border using both dimensions, just as the renderer does.
        assert!(scale.contains("Scale: 2.00 MiB/s"));

        g.history.clear();
        assert!(text(&g, ChartKind::Network, 5).contains("Scale: 256.0 KiB/s"));
    }

    #[test]
    fn history_follows_chart_width_and_keeps_the_newest_samples() {
        let mut g = Graphs::new();
        g.set_history_width(20);
        for _ in 0..100 {
            g.update(true);
        }
        assert_eq!(g.history.len(), 20);
        assert_eq!(g.history.front().unwrap().as_ref().unwrap().timestamp, 81.0);
        g.set_history_width(5);
        assert_eq!(g.history.len(), 5);
        assert_eq!(g.history.front().unwrap().as_ref().unwrap().timestamp, 96.0);
        assert!(g.history.capacity() < 20);
        g.set_history_width(10);
        assert_eq!(g.history.len(), 5); // Enlarging cannot restore discarded samples.
        g.push_sample(None, false);
        assert_eq!(g.history.len(), 6);
        assert!(g.history.back().unwrap().is_none());
        assert!(g.tooltips.iter().all(|t| t.contains("unavailable")));
        for _ in 0..10 {
            g.update(true);
        }
        assert_eq!(g.history.len(), 10);
        assert!(g.history.iter().all(Option::is_some));
        g.set_history_width(0);
        assert_eq!(g.history.len(), 1);
    }
}
