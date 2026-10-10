//! Per-domain swap display: a fixed percentage scale, never stacked percentages.
use super::*;
use multiload_qubes_snapshot::{SwapEntry, SwapSnapshot};
use std::collections::BTreeMap;

pub(crate) fn tooltip(swap: Option<&SwapSnapshot>) -> String {
    let Some(swap) = swap else {
        return "Swap: unavailable (legacy collector)".into();
    };
    let total: u128 = swap.entries.iter().map(|e| e.total as u128).sum();
    let used: u128 = swap.entries.iter().map(|e| e.used as u128).sum();
    let mut text = format!(
        "Swap: {:.2} / {:.2} GiB known used / enabled\nSwap reporting: {}/{} domains (including dom0)",
        used as f64 / 1048576.0, total as f64 / 1048576.0,
        swap.entries.len(), swap.expected
    );
    if swap.entries.len() < swap.expected as usize {
        text.push_str("\nMissing swap reports: totals and peak cover reporting domains only.");
    }
    let mut ranked: Vec<_> = swap.entries.iter().filter(|e| e.total > 0).collect();
    ranked.sort_by(|a, b| {
        b.percent()
            .unwrap()
            .total_cmp(&a.percent().unwrap())
            .then_with(|| b.used.cmp(&a.used))
            .then_with(|| a.name.cmp(&b.name))
    });
    if ranked.is_empty() {
        text.push_str("\nNo enabled swap among reporting domains.");
    } else {
        text.push_str("\nSwap highest users:");
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
    text.push_str("\nSwap chart: 100% = one domain's enabled swap; fill = highest percentage.");
    text
}

struct Trace<'a> {
    name: &'a str,
    points: Vec<Option<f64>>,
}

fn traces<'a>(samples: &[Option<&'a Snapshot>]) -> BTreeMap<&'a str, Trace<'a>> {
    let mut traces = BTreeMap::new();
    for (i, sample) in samples.iter().enumerate() {
        let Some(swap) = sample.and_then(|s| s.swap.as_ref()) else {
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

// Deterministic tints of multiload-ng's #9C43D1, stable across process restarts.
fn color(name: &str) -> u32 {
    let hash = name
        .bytes()
        .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    let tint = (hash % 6) as f64 * 0.09;
    [0x9c, 0x43, 0xd1].into_iter().fold(0, |rgb, c| {
        (rgb << 8) | (c as f64 + (255 - c) as f64 * tint) as u32
    })
}

impl Graphs {
    pub(super) unsafe fn swap_chart(&self, cr: *mut c_void, width: i32, height: i32) {
        let (w, h) = (width as f64, height as f64);
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
            let swap = sample.and_then(|s| s.swap.as_ref());
            if let Some(peak) = swap.and_then(SwapSnapshot::peak) {
                let size = peak / 100.0 * (h - 3.0);
                cairo_set_source_rgba(
                    cr,
                    0x9c as f64 / 255.0,
                    0x43 as f64 / 255.0,
                    0xd1 as f64 / 255.0,
                    0.25,
                );
                cairo_rectangle(cr, left + i as f64, h - 1.5 - size, 1.0, size);
                cairo_fill(cr);
            }
            if swap.is_none_or(|s| s.entries.len() < s.expected as usize) {
                rect(cr, 0x777777, left + i as f64, h - 3.0, 1.0, 2.0);
            }
        }
        cairo_save(cr);
        cairo_rectangle(cr, 1.0, 1.0, w - 2.0, h - 2.0);
        cairo_clip(cr);
        cairo_set_line_width(cr, 1.0);
        for trace in traces(&samples).values() {
            let rgb = color(trace.name);
            cairo_set_source_rgb(
                cr,
                ((rgb >> 16) & 255) as f64 / 255.0,
                ((rgb >> 8) & 255) as f64 / 255.0,
                (rgb & 255) as f64 / 255.0,
            );
            let mut previous = None;
            for (i, percent) in trace.points.iter().enumerate() {
                if let Some(percent) = percent {
                    let point = (left + i as f64 + 0.5, h - 1.5 - percent / 100.0 * (h - 3.0));
                    if let Some((x, y)) = previous {
                        cairo_move_to(cr, x, y);
                        cairo_line_to(cr, point.0, point.1);
                        cairo_stroke(cr);
                    } else {
                        rect(cr, rgb, point.0 - 0.5, point.1 - 0.5, 1.0, 1.0);
                    }
                    previous = Some(point);
                } else {
                    previous = None;
                }
            }
        }
        cairo_restore(cr);
    }
}

pub(crate) fn demo(t: f64) -> SwapSnapshot {
    SwapSnapshot {
        expected: 3,
        entries: [("dom0", 0.0), ("work", 0.9), ("dev", 2.1)]
            .into_iter()
            .map(|(name, phase)| SwapEntry {
                id: name.into(),
                name: name.into(),
                total: 1048576,
                used: ((0.45 + (t + phase).sin() * 0.4) * 1048576.0) as u64,
            })
            .collect(),
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
        let lines = traces(&samples);
        assert_eq!(
            lines["dom0"].points,
            vec![Some(60.0), None, Some(60.0), None]
        );
        assert_eq!(
            lines["new-incarnation"].points,
            vec![None, None, None, Some(60.0)]
        );
        assert_eq!(color("dom0"), color("dom0"));
    }
    #[test]
    fn tooltip_explains_partial_coverage_and_ranks_by_individual_pressure() {
        let mut s = demo(0.0);
        s.expected = 4;
        s.entries[0].total = 0;
        s.entries[0].used = 0;
        s.entries[1].total = 100;
        s.entries[1].used = 90;
        s.entries[2].total = 10000;
        s.entries[2].used = 5000;
        assert_eq!(s.peak(), Some(90.0));
        let text = tooltip(Some(&s));
        assert!(text.contains("3/4"));
        assert!(text.contains("reporting domains only"));
        assert!(text.find("work: 90.0%").unwrap() < text.find("dev: 50.0%").unwrap());
        assert!(tooltip(None).contains("unavailable"));
        s.entries.clear();
        assert_eq!(s.peak(), None);
        assert!(tooltip(Some(&s)).contains("No enabled swap"));
    }
}
