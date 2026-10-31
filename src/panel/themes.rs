//! Versioned GLib key files shared by panel instances. No GTK widgets here.
use super::config::Palettes;
use crate::palette::BuiltinTheme;
use gio::prelude::*;
use glib::KeyFile;
use std::{fs, io::Read, os::unix::fs::DirBuilderExt, path::PathBuf};

pub const BUILTINS: [(BuiltinTheme, &str); 5] = [
    (BuiltinTheme::Original, "Original"),
    (BuiltinTheme::LightGradient, "Light Gradient"),
    (BuiltinTheme::Tango, "Tango"),
    (BuiltinTheme::SolarizedDark, "Solarized Dark"),
    (BuiltinTheme::SolarizedLight, "Solarized Light"),
];
const GROUPS: [&str; 7] = [
    "CPU",
    "RAM",
    "Network",
    "Swap",
    "Disk",
    "PerQubeRAM",
    "PerQubeCPU",
];
const KEYS: [&str; 7] = [
    "Border",
    "BackgroundTop",
    "BackgroundBottom",
    "Series1",
    "Series2",
    "Series3",
    "Missing",
];
#[derive(Clone, Debug)]
pub struct SavedTheme {
    pub id: String,
    pub name: String,
    pub palettes: Palettes,
}
pub struct ThemeStore {
    pub dir: PathBuf,
    pub themes: Vec<SavedTheme>,
}
pub fn reserved_name(name: &str) -> bool {
    name == "Custom draft" || BUILTINS.iter().any(|(_, n)| *n == name)
}
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && name.chars().count() <= 128
        && !name.chars().any(char::is_control)
}
impl SavedTheme {
    fn decode(id: &str, data: &str) -> Result<Self, String> {
        if !glib::uuid_string_is_valid(id) {
            return Err("Invalid theme identifier".into());
        }
        let file = KeyFile::new();
        file.load_from_data(data, glib::KeyFileFlags::NONE)
            .map_err(|e| e.to_string())?;
        if file
            .integer("Theme", "Version")
            .map_err(|e| e.to_string())?
            != 1
        {
            return Err("Unsupported theme version".into());
        }
        let name = file
            .string("Theme", "Name")
            .map_err(|e| e.to_string())?
            .to_string();
        if !valid_name(&name) || reserved_name(&name) {
            return Err("Invalid or reserved theme name".into());
        }
        let mut palettes =
            std::array::from_fn(|k| crate::palette::Palette::preset(BuiltinTheme::Original, k));
        for (k, group) in GROUPS.iter().enumerate() {
            for (f, key) in KEYS.iter().enumerate() {
                let value = file.string(group, key).map_err(|e| e.to_string())?;
                if value.len() != 9
                    || !value.starts_with('#')
                    || !value[1..].bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err("Invalid RGBA color".into());
                }
                palettes[k].colors[f] =
                    u32::from_str_radix(&value[1..], 16).map_err(|e| e.to_string())?;
            }
            let width = file
                .integer(group, "BorderWidth")
                .map_err(|e| e.to_string())?;
            if !(0..=16).contains(&width) {
                return Err("Invalid border width".into());
            }
            palettes[k].border = width as u32;
        }
        Ok(Self {
            id: id.into(),
            name,
            palettes,
        })
    }
    fn encode(&self) -> KeyFile {
        let file = KeyFile::new();
        file.set_integer("Theme", "Version", 1);
        file.set_string("Theme", "Name", &self.name);
        for (k, group) in GROUPS.iter().enumerate() {
            for (key, value) in KEYS.iter().zip(self.palettes[k].colors) {
                file.set_string(group, key, &format!("#{value:08X}"));
            }
            file.set_integer(group, "BorderWidth", self.palettes[k].border as i32);
        }
        file
    }
}
impl ThemeStore {
    pub fn load() -> (Self, Vec<String>) {
        Self::load_from(glib::user_config_dir().join("multiload-qubes/themes"))
    }
    pub fn load_from(dir: PathBuf) -> (Self, Vec<String>) {
        let mut store = Self {
            dir,
            themes: Vec::new(),
        };
        let mut errors = Vec::new();
        let entries = match fs::read_dir(&store.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (store, errors),
            Err(e) => return (store, vec![e.to_string()]),
        };
        for entry in entries {
            let result = (|| {
                let path = entry.map_err(|e| e.to_string())?.path();
                if path.extension().and_then(|s| s.to_str()) != Some("theme") {
                    return Ok(None);
                }
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .ok_or("Invalid filename")?;
                if !path.is_file() {
                    return Err("Theme is not a regular file".into());
                }
                let mut data = String::new();
                fs::File::open(&path)
                    .map_err(|e| e.to_string())?
                    .take(65537)
                    .read_to_string(&mut data)
                    .map_err(|e| e.to_string())?;
                if data.len() > 65536 {
                    return Err("Theme file is too large".into());
                }
                let theme = SavedTheme::decode(id, &data)?;
                if store.by_name(&theme.name).is_some() {
                    return Err("Duplicate theme name".into());
                }
                Ok(Some(theme))
            })();
            match result {
                Ok(Some(theme)) => store.themes.push(theme),
                Ok(None) => (),
                Err(e) => errors.push(e),
            }
        }
        store.sort();
        (store, errors)
    }
    fn sort(&mut self) {
        self.themes.sort_by(|a, b| a.name.cmp(&b.name));
    }
    pub fn find(&self, id: &str) -> Option<&SavedTheme> {
        self.themes.iter().find(|t| t.id == id)
    }
    pub fn by_name(&self, name: &str) -> Option<&SavedTheme> {
        self.themes.iter().find(|t| t.name == name)
    }
    pub fn write(
        &mut self,
        id: Option<&str>,
        name: &str,
        palettes: Palettes,
    ) -> Result<String, String> {
        // Use an ID for the filename, not the display name. Renames then keep
        // panel references valid, and names can contain spaces or slashes.
        let id = id
            .map(str::to_owned)
            .unwrap_or_else(|| glib::uuid_string_random().to_string());
        if !glib::uuid_string_is_valid(&id) || !valid_name(name) || reserved_name(name) {
            return Err("Invalid or reserved theme name or identifier".into());
        }
        if self.by_name(name).is_some_and(|t| t.id != id) {
            return Err("A theme with that name already exists".into());
        }
        if palettes.iter().any(|p| p.border > 16) {
            return Err("Invalid border width".into());
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|e| format!("Cannot create theme directory: {e}"))?;
        let theme = SavedTheme {
            id: id.clone(),
            name: name.into(),
            palettes,
        };
        let data = theme.encode().to_data();
        // Replace the file in one step. Only update the in-memory copy after
        // the write succeeds, so a failed save leaves the old theme intact.
        gio::File::for_path(self.path(&id)?)
            .replace_contents(
                data.as_bytes(),
                None,
                false,
                gio::FileCreateFlags::PRIVATE,
                gio::Cancellable::NONE,
            )
            .map_err(|e| e.to_string())?;
        if let Some(old) = self.themes.iter_mut().find(|t| t.id == id) {
            *old = theme;
        } else {
            self.themes.push(theme);
        }
        self.sort();
        Ok(id)
    }
    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        match fs::remove_file(self.path(id)?) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
        self.themes.retain(|t| t.id != id);
        Ok(())
    }
    fn path(&self, id: &str) -> Result<PathBuf, String> {
        if !glib::uuid_string_is_valid(id) {
            return Err("Invalid theme identifier".into());
        }
        Ok(self.dir.join(format!("{id}.theme")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn theme_files_roundtrip_and_failed_writes_preserve_saved_theme() {
        let tmp =
            Temp(std::env::temp_dir().join(format!("mq-theme-{}", glib::uuid_string_random())));
        let (mut store, errors) = ThemeStore::load_from(tmp.0.join("themes"));
        assert!(errors.is_empty());
        let mut palettes =
            super::super::config::Appearance::builtin(BuiltinTheme::LightGradient).palettes;
        palettes[0].colors[3] = 0x33669980;
        let id = store.write(None, "Cream / Ω", palettes).unwrap();
        let (loaded, errors) = ThemeStore::load_from(store.dir.clone());
        assert!(errors.is_empty());
        assert_eq!(loaded.find(&id).unwrap().palettes, palettes);
        let mut invalid = palettes;
        invalid[0].border = 17;
        assert!(store.write(Some(&id), "Cream / Ω", invalid).is_err());
        assert_eq!(
            ThemeStore::load_from(store.dir.clone())
                .0
                .find(&id)
                .unwrap()
                .palettes,
            palettes
        );
        assert!(store.write(None, "Original", palettes).is_err());
        assert!(store.delete("../escape").is_err());
        store.write(Some(&id), "Renamed", palettes).unwrap();
        let file = store.path(&id).unwrap();
        let data = fs::read_to_string(&file).unwrap();
        fs::write(&file, data.replace("Version=1", "Version=2")).unwrap();
        assert_eq!(ThemeStore::load_from(store.dir.clone()).1.len(), 1);
        fs::write(&file, data.replace("#33669980", "#336699G0")).unwrap();
        assert_eq!(ThemeStore::load_from(store.dir.clone()).1.len(), 1);
        store.delete(&id).unwrap();
        assert!(ThemeStore::load_from(store.dir.clone()).0.themes.is_empty());
    }
}
