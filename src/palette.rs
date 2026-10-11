//! Renderer palettes with packed RRGGBBAA colors.

// Built-in IDs persisted in panel settings.
// Custom draft uses ID 1000 and is not a built-in palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(super) enum BuiltinTheme {
    Original = 0,
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
    pub colors: [u32; 7],
    pub border: u32,
}
impl Palette {
    pub fn preset(preset: BuiltinTheme, kind: usize) -> Self {
        if preset == BuiltinTheme::LightGradient {
            return Self::light_gradient(kind);
        }
        let (border, top, first, second, third) = match kind {
            1 => (0x008042, 0x13261D, 0x03964F, 0x43D18D, 0xD14343),
            5 => (0x008042, 0x13261D, 0x43D18D, 0x03964F, 0),
            2 => (0x807100, 0x262413, 0xE2CC05, 0x696018, 0),
            3 => (0x510080, 0x1F1326, 0x9C43D1, 0x9C43D1, 0),
            4 => (0x804200, 0x261D13, 0xED7A00, 0xFFAB73, 0),
            6 => (0x005D80, 0x132126, 0x48BDE6, 0x036F96, 0),
            _ => (0x005D80, 0x132126, 0x036F96, 0, 0),
        };
        let mut p = Self {
            colors: [border, top, 0, first, second, third, 0x777777].map(|rgb| (rgb << 8) | 255),
            border: 0,
        };
        if preset != BuiltinTheme::Original {
            let (background, edge, blue, green, yellow, orange, purple, red, spare) = match preset {
                BuiltinTheme::Tango => (
                    0x2E3436, 0x555753, 0x729FCF, 0x73D216, 0xEDD400, 0xF57900, 0xAD7FA8, 0xEF2929,
                    0xC8E7AC,
                ),
                BuiltinTheme::SolarizedDark => (
                    0x002B36, 0x586E75, 0x268BD2, 0x859900, 0xB58900, 0xCB4B16, 0x6C71C4, 0xDC322F,
                    0xA8B86B,
                ),
                BuiltinTheme::SolarizedLight => (
                    0xFDF6E3, 0x93A1A1, 0x268BD2, 0x859900, 0xB58900, 0xCB4B16, 0x6C71C4, 0xDC322F,
                    0xCBD3A1,
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
                2 if light => (0xB89C00, 0x696018),
                4 if light => (0x2AA198, 0x16776F),
                1 => (green, spare),
                5 => (green, green),
                2 => (yellow, orange),
                3 => (purple, purple),
                4 => (orange, yellow),
                _ => (blue, blue),
            };
            p.colors[3] = (a << 8) | 255;
            p.colors[4] = (b << 8) | 255;
            p.colors[5] = (red << 8) | 255;
            p.colors[6] = (edge << 8) | 255;
        }
        if matches!(kind, 3 | 5 | 6) {
            p.colors[4] = (p.colors[4] & !255) | 64;
        }
        p
    }
    fn light_gradient(kind: usize) -> Self {
        // Keep the per-chart fill opacities rather than imposing a shared default.
        let (first, second) = match kind {
            1 => (0x26A269FF, 0x8FF0A4FF), // RAM
            2 => (0xE5A50AFF, 0xFF7800FF), // Network
            3 => (0x6C71C4FF, 0x6C71C440), // Swap
            4 => (0x2AA198FF, 0x16776FFF), // Disk
            5 => (0x26A269FF, 0x9DD52F61), // PerQubeRAM
            6 => (0x268BD2FF, 0x268BD240), // PerQubeCPU
            _ => (0x268BD2FF, 0x268BD2FF), // CPU
        };
        Self {
            colors: [
                0x93A1A1FF, 0xF3E5C0FF, 0xFDF6E300, first, second, 0xDC322FFF, 0x93A1A1FF,
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
