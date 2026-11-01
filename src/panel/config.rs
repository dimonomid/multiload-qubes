//! Per-panel settings and the editor's theme selection, independent of GTK widgets.
//!
//! The panel keeps its own copy of the colors, even for a named theme. A draft
//! also keeps its starting colors so we can tell whether it has unsaved edits.
use super::charts::{ChartKind, CHARTS};
use super::palette::{BuiltinTheme, Palette};
use super::themes::ThemeStore;
use glib::KeyFile;
use std::path::Path;

const COLOR_KEYS: [&str; 7] = [
    "border",
    "background_top",
    "background_bottom",
    "series1",
    "series2",
    "series3",
    "missing",
];
pub type Palettes = [Palette; ChartKind::COUNT];

const SAVED_THEME_ID: i32 = 999;
const DRAFT_THEME_ID: i32 = 1000;

/// A draft may still point to the saved theme it came from. That link allows
/// Save changes to overwrite the original; it does not mean the draft is saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// Hardcoded preset; edits become a draft instead of changing the preset.
    Builtin(BuiltinTheme),
    /// Named theme UUID; its colors are also stored in the panel settings.
    Saved(String),
    /// Editable colors, optionally linked to a named theme for Save changes.
    Draft { source: Option<String> },
}
impl Selection {
    /// Combo-box key; saved files use UUIDs rather than built-in numeric IDs.
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
    /// Saved theme that Save changes should overwrite, if there is one.
    pub fn source(&self) -> Option<&str> {
        match self {
            Self::Saved(id) => Some(id),
            Self::Draft { source } => source.as_deref(),
            _ => None,
        }
    }
}
/// Colors being used, their theme selection, and the starting point for draft edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Appearance {
    /// Built-in, saved file, or editable draft; colors are also kept locally.
    pub selection: Selection,
    /// Current colors and borders for every chart, including disabled charts.
    pub palettes: Palettes,
    // Keep the starting colors so undoing an edit also clears the discard warning.
    baseline: Palettes,
}
impl Default for Appearance {
    fn default() -> Self {
        Self::builtin(BuiltinTheme::default())
    }
}
impl Appearance {
    pub fn builtin(theme: BuiltinTheme) -> Self {
        let palettes = std::array::from_fn(|k| Palette::preset(theme, ChartKind::ALL[k]));
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
    /// Keep these colors as the new baseline after the theme file was saved.
    pub fn mark_saved(&mut self, id: String) {
        self.selection = Selection::Saved(id);
        self.baseline = self.palettes;
    }
    /// Enter draft mode without changing colors or losing the saved source.
    pub fn edit(&mut self) {
        self.selection = Selection::Draft {
            source: self.selection.source().map(str::to_owned),
        };
    }
    /// Keep local colors as a draft if their saved theme was changed or deleted.
    pub fn validate_selection(&mut self, themes: &ThemeStore) {
        // A saved file may have changed or gone away. Keep the panel's colors
        // as a draft instead of replacing them with the current file contents.
        if let Selection::Saved(id) = &self.selection {
            if themes.find(id).is_none_or(|t| t.palettes != self.palettes) {
                self.edit();
            }
        }
    }
    /// Selecting draft alone is not a change; compare its colors with the baseline.
    pub fn unsaved(&self) -> bool {
        matches!(self.selection, Selection::Draft { .. }) && self.palettes != self.baseline
    }
}
/// Settings for one panel instance, kept separate from shared named theme files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Chart width in pixels; zero uses the panel size for square charts.
    pub width: i32,
    /// Visibility by ChartKind index, independent of display order.
    pub enabled: [bool; ChartKind::COUNT],
    /// Every chart exactly once, including disabled charts.
    pub order: [ChartKind; ChartKind::COUNT],
    /// Local theme choice, colors, and draft baseline.
    pub appearance: Appearance,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            width: 0,
            enabled: CHARTS.map(|chart| chart.default_enabled),
            order: ChartKind::ALL,
            appearance: Appearance::default(),
        }
    }
}
impl Config {
    /// Return enabled charts in the user's display order.
    pub fn visible_kinds(&self) -> Vec<ChartKind> {
        self.order
            .iter()
            .copied()
            .filter(|kind| self.enabled[kind.index()])
            .collect()
    }

    /// Repair duplicate order entries, invalid widths, and an empty chart selection.
    pub fn normalize(&mut self) {
        let mut order = Vec::with_capacity(ChartKind::COUNT);
        // Drop repeats, then append any missing charts.
        for i in self.order.into_iter().chain(ChartKind::ALL) {
            if !order.contains(&i) {
                order.push(i);
            }
        }
        self.order = order.try_into().unwrap();
        self.width = self.width.clamp(0, 512);
        if (1..4).contains(&self.width) {
            self.width = 4;
        }
        if !self.enabled.iter().any(|v| *v) {
            self.enabled = CHARTS.map(|chart| chart.default_enabled);
        }
    }
    pub fn load(path: &Path) -> Result<Self, glib::Error> {
        let file = KeyFile::new();
        file.load_from_file(path, glib::KeyFileFlags::NONE)?;
        Ok(Self::decode(&file))
    }
    /// Use defaults for absent or invalid settings, preserving valid saved choices.
    pub fn decode(file: &KeyFile) -> Self {
        let mut config = Self {
            width: file.integer("Graphs", "width").unwrap_or(0),
            ..Self::default()
        };
        for chart in &CHARTS {
            let enabled = &mut config.enabled[chart.kind.index()];
            *enabled = file.boolean("Graphs", chart.key).unwrap_or(*enabled);
        }
        if let Ok(order) = file.string("Graphs", "order") {
            let mut kinds = Vec::with_capacity(ChartKind::COUNT);
            for kind in order
                .split(',')
                .filter_map(|key| ChartKind::from_key(key.trim()))
                .chain(ChartKind::ALL)
            {
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
            config.order = kinds.try_into().unwrap();
        }
        let source = file
            .string("Appearance", "custom_theme_id")
            .ok()
            .map(|s| s.to_string())
            .filter(|id| glib::uuid_string_is_valid(id));
        config.appearance.selection = match file
            .integer("Appearance", "theme")
            .unwrap_or(BuiltinTheme::default() as i32)
        {
            SAVED_THEME_ID if source.is_some() => Selection::Saved(source.unwrap()),
            SAVED_THEME_ID | DRAFT_THEME_ID => Selection::Draft { source },
            id => Selection::Builtin(BuiltinTheme::from_id(id as u32).unwrap_or_default()),
        };
        if let Selection::Builtin(theme) = config.appearance.selection {
            config.appearance = Appearance::builtin(theme);
        }
        config.appearance.palettes = read_palettes(file, "Appearance", config.appearance.palettes);
        // Saving panel settings does not save a named theme. Keep the draft's
        // starting colors too, so reopening Preferences keeps its unsaved state.
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
            &self.order.map(|kind| kind.metadata().key).join(","),
        );
        file.set_integer("Graphs", "width", self.width);
        for chart in &CHARTS {
            file.set_boolean("Graphs", chart.key, self.enabled[chart.kind.index()]);
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

/// Overlay valid saved colors on defaults; panel settings tolerate missing fields.
fn read_palettes(file: &KeyFile, prefix: &str, mut palettes: Palettes) -> Palettes {
    for kind in ChartKind::ALL {
        let palette = &mut palettes[kind.index()];
        let group = format!("{prefix}-{}", kind.metadata().palette_id);
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
    for kind in ChartKind::ALL {
        let palette = &palettes[kind.index()];
        let group = format!("{prefix}-{}", kind.metadata().palette_id);
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
    fn default_charts_leave_per_qube_cpu_and_ram_available_and_keep_saved_choices() {
        let file = KeyFile::new();
        let defaults = Config::decode(&file);
        let disabled: Vec<_> = CHARTS
            .iter()
            .zip(defaults.enabled)
            .filter_map(|(name, enabled)| (!enabled).then_some(name.name))
            .collect();
        assert_eq!(disabled, ["Per-qube CPU", "Per-qube RAM"]);
        file.set_boolean("Graphs", "qube_cpu", true);
        file.set_boolean("Graphs", "ram", false);
        let loaded = Config::decode(&file);
        assert!(loaded.enabled[ChartKind::QubeCpu.index()]);
        assert!(!loaded.enabled[ChartKind::Ram.index()]);
        assert_eq!(Config::decode(&loaded.encode()), loaded);
    }
    #[test]
    fn default_theme_is_light_gradient_but_saved_choices_are_kept() {
        let expected = Appearance::builtin(BuiltinTheme::LightGradient);
        assert_eq!(Config::default().appearance, expected);
        let file = KeyFile::new();
        assert_eq!(Config::decode(&file).appearance, expected);
        file.set_integer("Appearance", "theme", -1);
        assert_eq!(Config::decode(&file).appearance, expected);
        file.set_integer("Appearance", "theme", BuiltinTheme::Original as i32);
        assert_eq!(
            Config::decode(&file).appearance,
            Appearance::builtin(BuiltinTheme::Original)
        );
    }
    #[test]
    fn chart_order_roundtrip_keeps_disabled_positions() {
        let config = Config {
            order: [
                ChartKind::Disk,
                ChartKind::Cpu,
                ChartKind::Network,
                ChartKind::Ram,
                ChartKind::QubeCpu,
                ChartKind::QubeRam,
                ChartKind::Swap,
            ],
            enabled: [true, false, true, true, true, true, false],
            ..Config::default()
        };
        let mut loaded = Config::decode(&config.encode());
        assert_eq!(loaded, config);
        assert_eq!(
            loaded.visible_kinds(),
            vec![
                ChartKind::Cpu,
                ChartKind::Network,
                ChartKind::Ram,
                ChartKind::QubeRam,
                ChartKind::Swap
            ]
        );
        loaded.enabled[ChartKind::Disk.index()] = true;
        assert_eq!(loaded.visible_kinds()[0], ChartKind::Disk);
    }
    #[test]
    fn missing_or_invalid_chart_orders_keep_every_chart_once() {
        let file = KeyFile::new();
        assert_eq!(Config::decode(&file).order, Config::default().order);
        file.set_string("Graphs", "order", "disk,cpu,disk,unknown,ram");
        assert_eq!(
            Config::decode(&file).order,
            [
                ChartKind::Disk,
                ChartKind::Cpu,
                ChartKind::Ram,
                ChartKind::QubeCpu,
                ChartKind::QubeRam,
                ChartKind::Network,
                ChartKind::Swap
            ]
        );
        let mut config = Config {
            order: [
                ChartKind::Network,
                ChartKind::Ram,
                ChartKind::Ram,
                ChartKind::Cpu,
                ChartKind::Cpu,
                ChartKind::Network,
                ChartKind::Ram,
            ],
            ..Config::default()
        };
        config.normalize();
        assert_eq!(
            config.order,
            [
                ChartKind::Network,
                ChartKind::Ram,
                ChartKind::Cpu,
                ChartKind::QubeCpu,
                ChartKind::QubeRam,
                ChartKind::Swap,
                ChartKind::Disk
            ]
        );
    }
    #[test]
    fn numbered_palette_groups_keep_their_existing_chart_meaning() {
        let file = KeyFile::new();
        // These group numbers are part of the existing settings format.
        let groups = [
            ("Appearance-0", ChartKind::Cpu),
            ("Appearance-1", ChartKind::Ram),
            ("Appearance-2", ChartKind::Network),
            ("Appearance-3", ChartKind::Swap),
            ("Appearance-4", ChartKind::Disk),
            ("Appearance-5", ChartKind::QubeRam),
            ("Appearance-6", ChartKind::QubeCpu),
        ];
        for (i, (group, _)) in groups.iter().enumerate() {
            file.set_uint64(group, "series1", (i + 100) as u64);
        }
        let loaded = Config::decode(&file);
        let saved = loaded.encode();
        for (i, (group, kind)) in groups.iter().enumerate() {
            let color = (i + 100) as u32;
            assert_eq!(loaded.appearance.palettes[kind.index()].colors[3], color);
            assert_eq!(saved.uint64(group, "series1").unwrap(), color as u64);
        }
    }
    #[test]
    fn existing_settings_and_draft_source_roundtrip() {
        let file = KeyFile::new();
        file.load_from_data("[Graphs]\nwidth=2\ninterval_ms=500\ncpu=false\nram=true\n[Appearance]\ntheme=1000\ncustom_theme_id=063e30ca-bfce-4149-98f2-b0f07f7fdaf2\n[Appearance-0]\nbackground_bottom=4260815616\n", glib::KeyFileFlags::NONE).unwrap();
        let config = Config::decode(&file);
        assert_eq!(config.width, 4);
        assert!(!config.enabled[ChartKind::Cpu.index()]);
        assert!(!config.enabled[ChartKind::QubeCpu.index()]);
        assert!(config.enabled[ChartKind::Ram.index()]);
        assert!(!config.appearance.unsaved());
        assert_eq!(
            config.appearance.palettes[ChartKind::Cpu.index()].colors[2],
            0xFDF6E300
        );
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
        appearance.palettes[ChartKind::QubeRam.index()].colors[3] ^= 255; // Opacity counts as an edit.
        assert!(appearance.unsaved());
        appearance.palettes[ChartKind::QubeRam.index()].colors[3] ^= 255;
        assert!(!appearance.unsaved());
        appearance.palettes[ChartKind::Cpu.index()].border = 3;
        assert!(appearance.unsaved());
        appearance.mark_saved("063e30ca-bfce-4149-98f2-b0f07f7fdaf2".into());
        appearance.edit();
        assert!(!appearance.unsaved());
        appearance.palettes[ChartKind::Cpu.index()].border = 0;
        assert!(appearance.unsaved());
        appearance.palettes[ChartKind::Cpu.index()].border = 3;
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
        config.appearance.palettes[ChartKind::QubeCpu.index()].border = 2;
        let mut loaded = Config::decode(&config.encode());
        assert_eq!(loaded, config);
        assert!(loaded.appearance.unsaved());
        loaded.appearance.palettes[ChartKind::QubeCpu.index()].border = 0;
        assert!(!loaded.appearance.unsaved());
    }
}
