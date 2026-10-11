//! Per-domain CPU, RAM and swap display: a fixed percentage scale, never stacked percentages.
use super::*;
use multiload_qubes_snapshot::{DomainUsage, DomainUsageSnapshot};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Metric {
    Swap,
    Ram,
    Cpu,
}
impl Metric {
    fn values(self, snapshot: &Snapshot) -> Option<&DomainUsageSnapshot> {
        match self {
            Self::Swap => snapshot.swap.as_ref(),
            Self::Ram => snapshot.qube_ram.as_ref(),
            Self::Cpu => snapshot.qube_cpu.as_ref().map(|c| &c.usage),
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Swap => "Swap",
            Self::Ram => "Qube RAM",
            Self::Cpu => "Qube CPU",
        }
    }
}

pub(crate) fn tooltip(usage: Option<&DomainUsageSnapshot>, metric: Metric) -> String {
    let label = metric.label();
    let Some(usage) = usage else {
        return format!("{label}: unavailable (legacy collector)");
    };
    let total: u128 = usage.entries.iter().map(|e| e.total as u128).sum();
    let used: u128 = usage.entries.iter().map(|e| e.used as u128).sum();
    let denominator = if metric == Metric::Ram {
        "guest-visible"
    } else {
        "enabled"
    };
    let mut text = format!("{label}: {:.2} / {:.2} GiB known used / {denominator}\n{label} reporting: {}/{} domains (including dom0)",
        used as f64 / 1048576.0, total as f64 / 1048576.0, usage.entries.len(), usage.expected);
    if usage.entries.len() < usage.expected as usize {
        text.push_str("\nMissing reports: totals and peak cover reporting domains only.");
    }
    let mut ranked: Vec<_> = usage.entries.iter().filter(|e| e.total > 0).collect();
    ranked.sort_by(|a, b| {
        b.percent()
            .unwrap()
            .total_cmp(&a.percent().unwrap())
            .then_with(|| b.used.cmp(&a.used))
            .then_with(|| a.name.cmp(&b.name))
    });
    if ranked.is_empty() {
        text.push_str(if metric == Metric::Swap {
            "\nNo enabled swap among reporting domains."
        } else {
            "\nNo RAM measurements available."
        });
    } else {
        text.push_str(&format!("\n{label} highest users:"));
        for e in ranked.into_iter().take(5) {
            text.push_str(&format!(
                "\n{}: {:.1}% ({:.0} / {:.0} MiB)",
                e.name,
                e.percent().unwrap(),
                e.used as f64 / 1024.0,
                e.total as f64 / 1024.0
            ));
        }
    }
    text.push_str(&format!(
        "\n100% = one domain's {denominator} {}. Fill = highest percentage.",
        if metric == Metric::Ram { "RAM" } else { "swap" }
    ));
    if metric == Metric::Ram {
        text.push_str("\nUsed = MemTotal − MemAvailable. Ballooning can change the percentage.");
    }
    text
}

pub(crate) fn cpu_tooltip(cpu: Option<&multiload_qubes_snapshot::DomainCpuSnapshot>) -> String {
    let Some(cpu) = cpu else {
        return "Qube CPU: unavailable or warming up".into();
    };
    let mut text = format!(
        "Qube CPU: reporting {}/{} domains (including dom0)",
        cpu.usage.entries.len(),
        cpu.usage.expected
    );
    let mut ranked: Vec<_> = cpu.usage.entries.iter().collect();
    ranked.sort_by(|a, b| {
        b.percent()
            .unwrap()
            .total_cmp(&a.percent().unwrap())
            .then_with(|| a.name.cmp(&b.name))
    });
    for e in ranked.into_iter().take(5) {
        text.push_str(&format!(
            "\n{}: {:.1}% ({} vCPUs; {:.1}% of host)",
            e.name,
            e.percent().unwrap(),
            cpu.vcpus(e),
            cpu.host_percent(e)
        ));
    }
    if cpu.usage.entries.len() < cpu.usage.expected as usize {
        text.push_str("\nMissing or warming-up domains are not treated as idle.");
    }
    text.push_str("\n100% = all of that domain's online vCPUs busy. Fill = highest percentage.\nMeasures scheduled CPU time, not time waiting for Xen to schedule it.");
    text
}

struct Trace<'a> {
    name: &'a str,
    points: Vec<Option<f64>>,
}

fn traces<'a>(samples: &[Option<&'a Snapshot>], metric: Metric) -> BTreeMap<&'a str, Trace<'a>> {
    let mut traces = BTreeMap::new();
    for (i, sample) in samples.iter().enumerate() {
        let Some(swap) = sample.and_then(|s| metric.values(s)) else {
            continue;
        };
        for entry in &swap.entries {
            let Some(percent) = entry.percent() else {
                continue;
            };
            let trace = traces.entry(entry.id.as_str()).or_insert_with(|| Trace {
                name: &entry.name,
                points: vec![None; samples.len()],
            });
            trace.points[i] = Some(percent);
        }
    }
    traces
}

// Stable tints of multiload-ng's swap purple or bright memory green.
fn color(name: &str, base: u32, background: u32) -> u32 {
    let hash = name
        .bytes()
        .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    let tint = (hash % 6) as f64 * 0.09;
    let target = if ((background >> 24) & 255)
        + ((background >> 16) & 255)
        + ((background >> 8) & 255)
        > 384
    {
        0
    } else {
        255
    };
    let rgb = [(base >> 24) & 255, (base >> 16) & 255, (base >> 8) & 255]
        .into_iter()
        .fold(0, |rgb, c| {
            (rgb << 8) | (c as f64 + (target as f64 - c as f64) * tint) as u32
        });
    (rgb << 8) | (base & 255)
}

// One-pixel strokes sit on pixel centers; both 0% and 100% stay inside the border.
fn point_y(percent: f64, height: f64) -> f64 {
    1.5 + ((1.0 - percent / 100.0) * (height - 3.0)).round()
}

impl Graphs {
    pub(super) fn domain_chart(
        &self,
        cr: &cairo::Context,
        width: i32,
        height: i32,
        palette: &Palette,
        metric: Metric,
    ) -> Result<(), cairo::Error> {
        let (w, h) = (width as f64, height as f64);
        cr.save()?;
        cr.set_antialias(cairo::Antialias::None);
        cr.rectangle(1.0, 1.0, w - 2.0, h - 2.0);
        cr.clip();
        let n = self.history.len().min((width - 2) as usize);
        let samples: Vec<_> = self
            .history
            .iter()
            .skip(self.history.len() - n)
            .map(Option::as_ref)
            .collect();
        let left = w - 1.0 - n as f64;
        // Fill each column under the highest reporting percentage, not a sum.
        for (i, sample) in samples.iter().enumerate() {
            let swap = sample.and_then(|s| metric.values(s));
            if let Some(peak) = swap
                .and_then(DomainUsageSnapshot::peak)
                .filter(|v| *v > 0.0)
            {
                let top = point_y(peak, h) - 0.5;
                source(cr, palette.colors[4]);
                cr.rectangle(left + i as f64, top, 1.0, h - 1.0 - top);
                cr.fill()?;
            }
            if swap.is_none_or(|s| s.entries.len() < s.expected as usize) {
                rect(cr, palette.colors[6], left + i as f64, h - 3.0, 1.0, 2.0)?;
            }
        }
        cr.set_line_width(1.0);
        for trace in traces(&samples, metric).values() {
            let rgb = color(trace.name, palette.colors[3], palette.colors[1]);
            source(cr, rgb);
            let mut drawing = false;
            for (i, percent) in trace.points.iter().enumerate() {
                if let Some(percent) = percent {
                    let point = (left + i as f64 + 0.5, point_y(*percent, h));
                    if drawing {
                        cr.line_to(point.0, point.1);
                    } else {
                        // Also show an isolated measurement with no adjacent sample.
                        rect(cr, rgb, point.0 - 0.5, point.1 - 0.5, 1.0, 1.0)?;
                        cr.move_to(point.0, point.1);
                    }
                    drawing = true;
                } else if drawing {
                    cr.stroke()?;
                    drawing = false;
                }
            }
            cr.stroke()?;
        }
        cr.restore()
    }
}

pub(crate) fn demo(t: f64, metric: Metric) -> DomainUsageSnapshot {
    DomainUsageSnapshot {
        expected: 3,
        entries: [("dom0", 0.0), ("work", 0.9), ("dev", 2.1)]
            .into_iter()
            .map(|(name, phase)| {
                let total = if metric == Metric::Ram {
                    4 * 1048576
                } else if metric == Metric::Cpu {
                    (if name == "dom0" { 16 } else { 4 }) * 1_000_000_000
                } else {
                    1048576
                };
                DomainUsage {
                    id: name.into(),
                    name: name.into(),
                    total,
                    used: ((0.45 + (t + phase).sin() * 0.4) * total as f64) as u64,
                }
            })
            .collect(),
    }
}

pub(crate) fn demo_cpu(t: f64) -> multiload_qubes_snapshot::DomainCpuSnapshot {
    multiload_qubes_snapshot::DomainCpuSnapshot {
        window_ns: 1_000_000_000,
        host_cpus: 16,
        usage: demo(t, Metric::Cpu),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn traces_keep_qubes_separate_and_break_on_missing_reports_and_restarts() {
        let mut graphs = Graphs::new();
        graphs.update(true);
        let mut first = graphs.history[0].clone().unwrap();
        let original = first.swap.as_mut().unwrap();
        original.entries.truncate(1);
        original.entries[0].total = 100;
        original.entries[0].used = 60;
        let mut restarted = first.clone();
        restarted.swap.as_mut().unwrap().entries[0].id = "new-incarnation".into();
        let samples = [Some(&first), None, Some(&first), Some(&restarted)];
        let lines = traces(&samples, Metric::Swap);
        assert_eq!(
            lines["dom0"].points,
            vec![Some(60.0), None, Some(60.0), None]
        );
        assert_eq!(
            lines["new-incarnation"].points,
            vec![None, None, None, Some(60.0)]
        );
        assert_eq!(
            color("dom0", 0x9C43D1FF, 0x1F1326FF),
            color("dom0", 0x9C43D1FF, 0x1F1326FF)
        );
    }
    #[test]
    fn ram_uses_current_totals_and_breaks_lines_for_missing_reports_and_restarts() {
        let mut g = Graphs::new();
        g.update(true);
        let mut first = g.history[0].clone().unwrap();
        let ram = first.qube_ram.as_mut().unwrap();
        ram.expected = 2;
        ram.entries.truncate(1);
        ram.entries[0].total = 1000;
        ram.entries[0].used = 400;
        let mut ballooned = first.clone();
        ballooned.qube_ram.as_mut().unwrap().entries[0].total = 500;
        let mut restarted = ballooned.clone();
        restarted.qube_ram.as_mut().unwrap().entries[0].id = "new".into();
        let samples = [Some(&first), Some(&ballooned), None, Some(&restarted)];
        let lines = traces(&samples, Metric::Ram);
        assert_eq!(
            lines["dom0"].points,
            vec![Some(40.0), Some(80.0), None, None]
        );
        assert_eq!(lines["new"].points, vec![None, None, None, Some(80.0)]);
        let text = tooltip(ballooned.qube_ram.as_ref(), Metric::Ram);
        assert!(text.starts_with("Qube RAM:"));
        assert!(text.contains("1/2") && text.contains("dom0: 80.0%"));
        assert!(text.contains("Ballooning") && text.contains("guest-visible"));
        assert!(tooltip(None, Metric::Ram).contains("unavailable"));
    }

    #[test]
    fn cpu_lines_break_on_gaps_and_tooltip_shows_qube_and_host_percentages() {
        let mut g = Graphs::new();
        g.update(true);
        let mut sample = g.history[0].clone().unwrap();
        let cpu = sample.qube_cpu.as_mut().unwrap();
        cpu.usage.entries.truncate(1);
        cpu.usage.entries[0].total = 4_000_000_000;
        cpu.usage.entries[0].used = 2_800_000_000;
        let text = cpu_tooltip(Some(cpu));
        assert!(text.contains("70.0% (4 vCPUs; 17.5% of host)"));
        assert!(text.contains("1/3") && text.contains("warming-up"));
        assert!(cpu_tooltip(None).contains("warming up"));
        let mut restarted = sample.clone();
        restarted.qube_cpu.as_mut().unwrap().usage.entries[0].id = "restarted".into();
        let lines = traces(&[Some(&sample), None, Some(&restarted)], Metric::Cpu);
        assert_eq!(lines["dom0"].points, vec![Some(70.0), None, None]);
        assert_eq!(lines["restarted"].points, vec![None, None, Some(70.0)]);
    }

    #[test]
    fn tooltip_explains_partial_coverage_and_ranks_by_individual_pressure() {
        let mut s = demo(0.0, Metric::Swap);
        s.expected = 4;
        s.entries[0].total = 0;
        s.entries[0].used = 0;
        s.entries[1].total = 100;
        s.entries[1].used = 90;
        s.entries[2].total = 10000;
        s.entries[2].used = 5000;
        assert_eq!(s.peak(), Some(90.0));
        let text = tooltip(Some(&s), Metric::Swap);
        assert!(text.contains("3/4"));
        assert!(text.contains("reporting domains only"));
        assert!(text.find("work: 90.0%").unwrap() < text.find("dev: 50.0%").unwrap());
        assert!(tooltip(None, Metric::Swap).contains("unavailable"));
        s.entries.clear();
        assert_eq!(s.peak(), None);
        assert!(tooltip(Some(&s), Metric::Swap).contains("No enabled swap"));
    }
}
