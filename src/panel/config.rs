//! Per-panel settings and the editor's theme selection, independent of GTK widgets.
use super::themes::ThemeStore;
use crate::palette::{BuiltinTheme, Palette};
use glib::KeyFile;
use std::path::Path;

pub const CHART_NAMES: [&str; 7] = [
    "CPU",
    "Per-qube CPU",
    "RAM",
    "Per-qube RAM",
    "Network",
    "Per-qube swap",
    "Disk I/O",
];
pub const CHART_KINDS: [usize; 7] = [0, 6, 1, 5, 2, 3, 4];
const CHART_KEYS: [&str; 7] = [
    "cpu", "qube_cpu", "ram", "qube_ram", "network", "swap", "disk",
];
const COLOR_KEYS: [&str; 7] = [
    "border",
    "background_top",
    "background_bottom",
    "series1",
    "series2",
    "series3",
    "missing",
];
pub type Palettes = [Palette; 7];

const SAVED_THEME_ID: i32 = 999;
const DRAFT_THEME_ID: i32 = 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    Builtin(BuiltinTheme),
    Saved(String),
    Draft { source: Option<String> },
}
impl Selection {
    pub fn id(&self) -> String {
        match self {
            Self::Saved(id) => format!("saved:{id}"),
            _ => self.persisted_id().to_string(),
        }
    }
    fn persisted_id(&self) -> i32 {
        match self {
            Self::Builtin(theme) => *theme as i32,
            Self::Saved(_) => SAVED_THEME_ID,
            Self::Draft { .. } => DRAFT_THEME_ID,
        }
    }
    pub fn source(&self) -> Option<&str> {
        match self {
            Self::Saved(id) => Some(id),
            Self::Draft { source } => source.as_deref(),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Appearance {
    pub selection: Selection,
    pub palettes: Palettes,
    baseline: Palettes,
}
impl Default for Appearance {
    fn default() -> Self {
        Self::builtin(BuiltinTheme::Original)
    }
}
impl Appearance {
    pub fn builtin(theme: BuiltinTheme) -> Self {
        let palettes = std::array::from_fn(|k| Palette::preset(theme, k));
        Self {
            selection: Selection::Builtin(theme),
            palettes,
            baseline: palettes,
        }
    }
    pub fn saved(id: String, palettes: Palettes) -> Self {
        Self {
            selection: Selection::Saved(id),
            palettes,
            baseline: palettes,
        }
    }
    pub fn mark_saved(&mut self, id: String) {
        self.selection = Selection::Saved(id);
        self.baseline = self.palettes;
    }
    pub fn edit(&mut self) {
        self.selection = Selection::Draft {
            source: self.selection.source().map(str::to_owned),
        };
    }
    pub fn validate_selection(&mut self, themes: &ThemeStore) {
        if let Selection::Saved(id) = &self.selection {
            if themes.find(id).is_none_or(|t| t.palettes != self.palettes) {
                self.edit();
            }
        }
    }
    pub fn unsaved(&self) -> bool {
        matches!(self.selection, Selection::Draft { .. }) && self.palettes != self.baseline
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub width: i32,
    pub interval: u32,
    pub enabled: [bool; 7],
    pub order: [usize; 7],
    pub appearance: Appearance,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            width: 0,
            interval: 1000,
            enabled: [true; 7],
            order: [0, 1, 2, 3, 4, 5, 6],
            appearance: Appearance::default(),
        }
    }
}
impl Config {
    pub fn visible_kinds(&self) -> Vec<usize> {
        self.order
            .iter()
            .copied()
            .filter(|&i| self.enabled[i])
            .map(|i| CHART_KINDS[i])
            .collect()
    }

    pub fn normalize(&mut self) {
        let mut order = Vec::with_capacity(7);
        for i in self.order.into_iter().chain(0..7) {
            if i < 7 && !order.contains(&i) {
                order.push(i);
            }
        }
        self.order = order.try_into().unwrap();
        self.width = self.width.clamp(0, 512);
        if (1..4).contains(&self.width) {
            self.width = 4;
        }
        self.interval = self.interval.clamp(250, 10000);
        if !self.enabled.iter().any(|v| *v) {
            self.enabled[..3].fill(true);
        }
    }
    pub fn load(path: &Path) -> Result<Self, glib::Error> {
        let file = KeyFile::new();
        file.load_from_file(path, glib::KeyFileFlags::NONE)?;
        Ok(Self::decode(&file))
    }
    pub fn decode(file: &KeyFile) -> Self {
        let mut config = Self {
            width: file.integer("Graphs", "width").unwrap_or(0),
            interval: file
                .integer("Graphs", "interval_ms")
                .unwrap_or(1000)
                .clamp(250, 10000) as u32,
            ..Self::default()
        };
        for (i, key) in CHART_KEYS.iter().enumerate() {
            config.enabled[i] = file.boolean("Graphs", key).unwrap_or(true);
        }
        if let Ok(order) = file.string("Graphs", "order") {
            config.order = [usize::MAX; 7];
            for (slot, index) in config
                .order
                .iter_mut()
                .zip(order.split(',').filter_map(|key| {
                    CHART_KEYS
                        .iter()
                        .position(|candidate| *candidate == key.trim())
                }))
            {
                *slot = index;
            }
        }
        let source = file
            .string("Appearance", "custom_theme_id")
            .ok()
            .map(|s| s.to_string())
            .filter(|id| glib::uuid_string_is_valid(id));
        config.appearance.selection = match file
            .integer("Appearance", "theme")
            .unwrap_or(BuiltinTheme::Original as i32)
        {
            SAVED_THEME_ID if source.is_some() => Selection::Saved(source.unwrap()),
            SAVED_THEME_ID | DRAFT_THEME_ID => Selection::Draft { source },
            id => Selection::Builtin(
                BuiltinTheme::from_id(id as u32).unwrap_or(BuiltinTheme::Original),
            ),
        };
        config.appearance.palettes = read_palettes(file, "Appearance", config.appearance.palettes);
        config.appearance.baseline =
            if matches!(config.appearance.selection, Selection::Draft { .. }) {
                read_palettes(file, "DraftBaseline", config.appearance.palettes)
            } else {
                config.appearance.palettes
            };
        config.normalize();
        config
    }
    pub fn encode(&self) -> KeyFile {
        let file = KeyFile::new();
        file.set_string(
            "Graphs",
            "order",
            &self.order.map(|i| CHART_KEYS[i]).join(","),
        );
        file.set_integer("Graphs", "width", self.width);
        file.set_integer("Graphs", "interval_ms", self.interval as i32);
        for (key, value) in CHART_KEYS.iter().zip(self.enabled) {
            file.set_boolean("Graphs", key, value);
        }
        file.set_integer(
            "Appearance",
            "theme",
            self.appearance.selection.persisted_id(),
        );
        if let Some(source) = self.appearance.selection.source() {
            file.set_string("Appearance", "custom_theme_id", source);
        }
        write_palettes(&file, "Appearance", &self.appearance.palettes);
        if matches!(self.appearance.selection, Selection::Draft { .. }) {
            write_palettes(&file, "DraftBaseline", &self.appearance.baseline);
        }
        file
    }
    pub fn save(&self, path: &Path) -> Result<(), glib::Error> {
        self.encode().save_to_file(path)
    }
}

fn read_palettes(file: &KeyFile, prefix: &str, mut palettes: Palettes) -> Palettes {
    for (k, palette) in palettes.iter_mut().enumerate() {
        let group = format!("{prefix}-{k}");
        for (f, key) in COLOR_KEYS.iter().enumerate() {
            if let Ok(value) = file.uint64(&group, key) {
                if let Ok(value) = u32::try_from(value) {
                    palette.colors[f] = value;
                }
            }
        }
        if let Ok(width) = file.uint64(&group, "border_width") {
            palette.border = width.min(16) as u32;
        }
    }
    palettes
}
fn write_palettes(file: &KeyFile, prefix: &str, palettes: &Palettes) {
    for (k, palette) in palettes.iter().enumerate() {
        let group = format!("{prefix}-{k}");
        for (key, color) in COLOR_KEYS.iter().zip(palette.colors) {
            file.set_uint64(&group, key, color as u64);
        }
        file.set_uint64(&group, "border_width", palette.border as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chart_order_roundtrip_keeps_disabled_positions() {
        let config = Config {
            order: [6, 0, 4, 2, 1, 3, 5],
            enabled: [true, false, true, true, true, true, false],
            ..Config::default()
        };
        let mut loaded = Config::decode(&config.encode());
        assert_eq!(loaded, config);
        assert_eq!(loaded.visible_kinds(), vec![0, 2, 1, 5, 3]);
        loaded.enabled[6] = true;
        assert_eq!(loaded.visible_kinds()[0], CHART_KINDS[6]);
    }
    #[test]
    fn missing_or_invalid_chart_orders_keep_every_chart_once() {
        let file = KeyFile::new();
        assert_eq!(Config::decode(&file).order, Config::default().order);
        file.set_string("Graphs", "order", "disk,cpu,disk,unknown,ram");
        assert_eq!(Config::decode(&file).order, [6, 0, 2, 1, 3, 4, 5]);
        let mut config = Config {
            order: [99, 4, 4, 2, 2, 0, 99],
            ..Config::default()
        };
        config.normalize();
        assert_eq!(config.order, [4, 2, 0, 1, 3, 5, 6]);
    }
    #[test]
    fn existing_settings_and_draft_source_roundtrip() {
        let file = KeyFile::new();
        file.load_from_data("[Graphs]\nwidth=2\ninterval_ms=500\ncpu=false\nram=true\n[Appearance]\ntheme=1000\ncustom_theme_id=063e30ca-bfce-4149-98f2-b0f07f7fdaf2\n[Appearance-0]\nbackground_bottom=4260815616\n", glib::KeyFileFlags::NONE).unwrap();
        let config = Config::decode(&file);
        assert_eq!(config.width, 4);
        assert!(!config.enabled[0]);
        assert!(config.enabled[1]);
        assert!(!config.appearance.unsaved());
        assert_eq!(config.appearance.palettes[0].colors[2], 0xFDF6E300);
        assert_eq!(Config::decode(&config.encode()), config);
        let mut saved = config;
        saved
            .appearance
            .mark_saved(saved.appearance.selection.source().unwrap().into());
        assert_eq!(Config::decode(&saved.encode()), saved);
    }
    #[test]
    fn draft_is_dirty_only_while_palette_differs_from_its_baseline() {
        let mut appearance = Appearance::builtin(BuiltinTheme::LightGradient);
        appearance.edit();
        assert!(!appearance.unsaved());
        appearance.palettes[5].colors[3] ^= 255; // Opacity counts as an edit.
        assert!(appearance.unsaved());
        appearance.palettes[5].colors[3] ^= 255;
        assert!(!appearance.unsaved());
        appearance.palettes[0].border = 3;
        assert!(appearance.unsaved());
        appearance.mark_saved("063e30ca-bfce-4149-98f2-b0f07f7fdaf2".into());
        appearance.edit();
        assert!(!appearance.unsaved());
        appearance.palettes[0].border = 0;
        assert!(appearance.unsaved());
        appearance.palettes[0].border = 3;
        assert!(!appearance.unsaved());
    }
    #[test]
    fn applying_and_reloading_a_draft_preserves_its_baseline() {
        let mut config = Config {
            appearance: Appearance::builtin(BuiltinTheme::SolarizedLight),
            ..Config::default()
        };
        config.appearance.edit();
        assert!(!Config::decode(&config.encode()).appearance.unsaved());
        config.appearance.palettes[6].border = 2;
        let mut loaded = Config::decode(&config.encode());
        assert_eq!(loaded, config);
        assert!(loaded.appearance.unsaved());
        loaded.appearance.palettes[6].border = 0;
        assert!(!loaded.appearance.unsaved());
    }
}
