//! Renderer palettes with packed RRGGBBAA colors.

use super::charts::ChartKind;

// Built-in IDs persisted in panel settings.
// Custom draft uses ID 1000 and is not a built-in palette.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u32)]
pub(super) enum BuiltinTheme {
    Original = 0,
    #[default]
    LightGradient = 1,
    Tango = 2,
    SolarizedDark = 3,
    SolarizedLight = 4,
}
impl BuiltinTheme {
    pub(super) fn from_id(id: u32) -> Option<Self> {
        match id {
            id if id == Self::Original as u32 => Some(Self::Original),
            id if id == Self::LightGradient as u32 => Some(Self::LightGradient),
            id if id == Self::Tango as u32 => Some(Self::Tango),
            id if id == Self::SolarizedDark as u32 => Some(Self::SolarizedDark),
            id if id == Self::SolarizedLight as u32 => Some(Self::SolarizedLight),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Palette {
    // Border, background top/bottom, three series, then missing data.
    // Per-qube charts use series 1 for lines and series 2 for the peak fill.
    pub colors: [u32; 7],
    pub border: u32,
}
impl Palette {
    pub fn preset(preset: BuiltinTheme, kind: ChartKind) -> Self {
        if preset == BuiltinTheme::LightGradient {
            return Self::light_gradient(kind);
        }
        let (border, top, first, second, third) = match kind {
            ChartKind::Ram => (0x008042, 0x13261D, 0x03964F, 0x43D18D, 0x718078),
            ChartKind::QubeRam => (0x008042, 0x13261D, 0x43D18D, 0x03964F, 0),
            ChartKind::Network => (0x807100, 0x262413, 0xE2CC05, 0x696018, 0),
            ChartKind::Swap => (0x510080, 0x1F1326, 0x9C43D1, 0x9C43D1, 0),
            ChartKind::Disk => (0x804200, 0x261D13, 0xED7A00, 0xFFAB73, 0),
            ChartKind::QubeCpu => (0x005D80, 0x132126, 0x48BDE6, 0x036F96, 0),
            ChartKind::Cpu => (0x005D80, 0x132126, 0x036F96, 0, 0),
        };
        let mut p = Self {
            colors: [border, top, 0, first, second, third, 0x777777].map(|rgb| (rgb << 8) | 255),
            border: 0,
        };
        if preset != BuiltinTheme::Original {
            let (background, edge, blue, green, yellow, orange, purple, unknown, spare) =
                match preset {
                    BuiltinTheme::Tango => (
                        0x2E3436, 0x555753, 0x729FCF, 0x73D216, 0xEDD400, 0xF57900, 0xAD7FA8,
                        0x888A85, 0xC8E7AC,
                    ),
                    BuiltinTheme::SolarizedDark => (
                        0x002B36, 0x586E75, 0x268BD2, 0x859900, 0xB58900, 0xCB4B16, 0x6C71C4,
                        0x839496, 0xA8B86B,
                    ),
                    BuiltinTheme::SolarizedLight => (
                        0xFDF6E3, 0x93A1A1, 0x268BD2, 0x859900, 0xB58900, 0xCB4B16, 0x6C71C4,
                        0x657B83, 0xCBD3A1,
                    ),
                    BuiltinTheme::Original | BuiltinTheme::LightGradient => {
                        unreachable!("handled above")
                    }
                };
            p.colors[0] = (edge << 8) | 255;
            p.colors[1] = (background << 8) | 255;
            p.colors[2] = p.colors[1];
            let light = preset == BuiltinTheme::SolarizedLight;
            let (a, b) = match kind {
                ChartKind::Network if light => (0xB89C00, 0x696018),
                ChartKind::Disk if light => (0x2AA198, 0x16776F),
                ChartKind::Ram => (green, spare),
                ChartKind::QubeRam => (green, green),
                ChartKind::Network => (yellow, orange),
                ChartKind::Swap => (purple, purple),
                ChartKind::Disk => (orange, yellow),
                ChartKind::Cpu | ChartKind::QubeCpu => (blue, blue),
            };
            p.colors[3] = (a << 8) | 255;
            p.colors[4] = (b << 8) | 255;
            p.colors[5] = (unknown << 8) | 255;
            p.colors[6] = (edge << 8) | 255;
        }
        if kind.is_per_qube() {
            p.colors[4] = (p.colors[4] & !255) | 64;
        }
        p
    }
    fn light_gradient(kind: ChartKind) -> Self {
        // Keep the per-chart fill opacities rather than imposing a shared default.
        let (first, second) = match kind {
            ChartKind::Ram => (0x26A269FF, 0x8FF0A4FF),
            ChartKind::Network => (0xE5A50AFF, 0xFF7800FF),
            ChartKind::Swap => (0x6C71C4FF, 0x6C71C440),
            ChartKind::Disk => (0x2AA198FF, 0x16776FFF),
            ChartKind::QubeRam => (0x26A269FF, 0x9DD52F61),
            ChartKind::QubeCpu => (0x268BD2FF, 0x268BD240),
            ChartKind::Cpu => (0x268BD2FF, 0x268BD2FF),
        };
        Self {
            colors: [
                0x93A1A1FF, 0xF3E5C0FF, 0xFDF6E300, first, second, 0x72776BFF, 0x93A1A1FF,
            ],
            border: 0,
        }
    }
}

pub(super) fn source(cr: &cairo::Context, color: u32) {
    cr.set_source_rgba(
        (color >> 24) as f64 / 255.0,
        ((color >> 16) & 255) as f64 / 255.0,
        ((color >> 8) & 255) as f64 / 255.0,
        (color & 255) as f64 / 255.0,
    );
}

pub(super) fn mix(a: u32, b: u32, t: f64) -> u32 {
    [24, 16, 8, 0].into_iter().fold(0, |out, shift| {
        let start = ((a >> shift) & 255) as f64;
        let end = ((b >> shift) & 255) as f64;
        (out << 8) | (start + (end - start) * t).round() as u32
    })
}

#[cfg(test)]
mod tests {
    use super::mix;
    #[test]
    fn rgba_interpolation_preserves_transparency_and_endpoints() {
        assert_eq!(mix(0xFF000000, 0x0000FFFF, 0.0), 0xFF000000);
        assert_eq!(mix(0xFF000000, 0x0000FFFF, 1.0), 0x0000FFFF);
        assert_eq!(mix(0xFF000000, 0x0000FFFF, 0.5), 0x80008080);
    }
}
