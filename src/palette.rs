//! Renderer palette. Colors cross the C ABI as packed RRGGBBAA integers.
use super::*;

// Stable IDs shared with Theme in native/plugin.c and persisted panel settings.
// Custom draft uses ID 1000 and is not a built-in palette.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(super) enum BuiltinTheme {
    Original = 0,
    Tango = 1,
    SolarizedDark = 2,
    SolarizedLight = 3,
    SolarizedLightGradient = 4,
}
impl BuiltinTheme {
    fn from_id(id: u32) -> Option<Self> {
        match id {
            0 => Some(Self::Original),
            1 => Some(Self::Tango),
            2 => Some(Self::SolarizedDark),
            3 => Some(Self::SolarizedLight),
            4 => Some(Self::SolarizedLightGradient),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub colors: [u32; 7],
    pub border: u32,
}
impl Palette {
    pub fn preset(preset: BuiltinTheme, kind: usize) -> Self {
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
                BuiltinTheme::SolarizedLight | BuiltinTheme::SolarizedLightGradient => (
                    0xFDF6E3, 0x93A1A1, 0x268BD2, 0x859900, 0xB58900, 0xCB4B16, 0x6C71C4, 0xDC322F,
                    0xCBD3A1,
                ),
                BuiltinTheme::Original => unreachable!("original palette is handled above"),
            };
            p.colors[0] = (edge << 8) | 255;
            p.colors[1] = (background << 8) | 255;
            p.colors[2] = p.colors[1];
            let (a, b) = match kind {
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
        if preset == BuiltinTheme::SolarizedLightGradient {
            p.colors[1] = 0xF3E5C0FF;
            p.colors[2] = 0xFDF6E300;
        }
        if matches!(kind, 3 | 5 | 6) {
            p.colors[4] = (p.colors[4] & !255) | 64;
        }
        p
    }
}

pub(super) unsafe fn source(cr: *mut c_void, color: u32) {
    cairo_set_source_rgba(
        cr,
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

/// # Safety
/// `g` must be a live graph handle on the GTK thread.
#[no_mangle]
pub unsafe extern "C" fn mq_palette_get(g: *const Graphs, kind: u32, field: u32) -> u32 {
    let Some(p) = g.as_ref().and_then(|g| g.palette.get(kind as usize)) else {
        return 0;
    };
    if field == 7 {
        p.border
    } else {
        p.colors.get(field as usize).copied().unwrap_or(0)
    }
}
/// # Safety
/// `g` must be a live, exclusively borrowed graph handle on the GTK thread.
#[no_mangle]
pub unsafe extern "C" fn mq_palette_set(g: *mut Graphs, kind: u32, field: u32, value: u32) {
    let Some(p) = g.as_mut().and_then(|g| g.palette.get_mut(kind as usize)) else {
        return;
    };
    if field == 7 {
        p.border = value.min(16);
    } else if let Some(color) = p.colors.get_mut(field as usize) {
        *color = value;
    }
}

#[no_mangle]
pub extern "C" fn mq_palette_preset(preset: u32, kind: u32, field: u32) -> u32 {
    let p = Palette::preset(
        BuiltinTheme::from_id(preset).unwrap_or(BuiltinTheme::Original),
        kind as usize,
    );
    if field == 7 {
        p.border
    } else {
        p.colors.get(field as usize).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rgba_interpolation_preserves_transparency_and_endpoints() {
        assert_eq!(mix(0xFF000000, 0x0000FFFF, 0.0), 0xFF000000);
        assert_eq!(mix(0xFF000000, 0x0000FFFF, 1.0), 0x0000FFFF);
        assert_eq!(mix(0xFF000000, 0x0000FFFF, 0.5), 0x80008080);
    }
}
