//! Файл настроек config.toml (лежит рядом с .exe).
//!
//! Читается через serde, а сохраняется через toml_edit — так комментарии
//! и порядок строк в файле остаются как были.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, Value};

use crate::engine::AudioSettings;
use crate::lag::LagMode;
use crate::{keys, notes};

/// Шаблон файла настроек с комментариями.
pub const TEMPLATE: &str = include_str!("default_config.toml");

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub audio: AudioCfg,
    pub lag: LagCfg,
    pub hotkey: HotkeyCfg,
    pub piano: PianoCfg,
    pub midi: MidiCfg,
    /// Нота ("C4") -> клавиша ("Q").
    pub keymap: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct PianoCfg {
    pub enabled: bool,
    pub soundfont: String,
    pub instrument: u8,
    pub volume: u32,
    pub monitor: bool,
    pub through_lag: bool,
    pub transpose: i32,
}

impl Default for PianoCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            soundfont: String::new(),
            instrument: 0,
            volume: 70,
            monitor: true,
            through_lag: false,
            transpose: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(default)]
pub struct MidiCfg {
    pub device: String,
    pub send_keys: bool,
    pub pedal_key: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct AudioCfg {
    pub input_device: String,
    pub output_device: String,
    pub input_gain_db: f32,
    pub monitor: bool,
    pub monitor_device: String,
    pub monitor_volume: u32,
    pub buffer_ms: u32,
}

impl Default for AudioCfg {
    fn default() -> Self {
        Self {
            input_device: String::new(),
            output_device: "CABLE Input".into(),
            input_gain_db: 0.0,
            monitor: false,
            monitor_device: String::new(),
            monitor_volume: 80,
            buffer_ms: 30,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Catchup {
    #[default]
    Speedup,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct LagCfg {
    pub mode: LagMode,
    pub intensity: u32,
    pub catchup: Catchup,
    pub catchup_speed: f32,
    pub bitrate: i32,
}

impl Default for LagCfg {
    fn default() -> Self {
        Self {
            mode: LagMode::Mix,
            intensity: 60,
            catchup: Catchup::Speedup,
            catchup_speed: 1.6,
            bitrate: 64_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HotkeyMode {
    #[default]
    Hold,
    Toggle,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct HotkeyCfg {
    pub key: String,
    pub mode: HotkeyMode,
}

impl Default for HotkeyCfg {
    fn default() -> Self {
        Self {
            key: "F8".into(),
            mode: HotkeyMode::Hold,
        }
    }
}

impl Config {
    /// Приводит значения к допустимым диапазонам.
    pub fn sanitize(&mut self) {
        let a = &mut self.audio;
        a.input_gain_db = a.input_gain_db.clamp(-24.0, 36.0);
        a.monitor_volume = a.monitor_volume.min(100);
        a.buffer_ms = a.buffer_ms.clamp(10, 300);
        let l = &mut self.lag;
        l.intensity = l.intensity.min(100);
        l.catchup_speed = l.catchup_speed.clamp(1.1, 3.0);
        l.bitrate = l.bitrate.clamp(6_000, 256_000);
        let p = &mut self.piano;
        p.instrument = p.instrument.min(127);
        p.volume = p.volume.min(100);
        p.transpose = p.transpose.clamp(-24, 24);
    }

    pub fn audio_settings(&self) -> AudioSettings {
        AudioSettings {
            input_device: self.audio.input_device.clone(),
            output_device: self.audio.output_device.clone(),
            monitor: self.audio.monitor,
            monitor_piano: self.piano.monitor,
            monitor_device: self.audio.monitor_device.clone(),
            buffer_ms: self.audio.buffer_ms,
            piano: self.piano.enabled,
            soundfont: self.piano.soundfont.clone(),
        }
    }

    /// Привязки в виде таблицы нота -> код клавиши. Второе значение — строки,
    /// которые не удалось разобрать (для предупреждения).
    pub fn keymap_table(&self) -> ([u16; 128], Vec<String>) {
        let mut map = [0u16; 128];
        let mut bad = Vec::new();
        for (note, key) in &self.keymap {
            match (notes::parse_note(note), keys::parse_key(key)) {
                (Some(n), Some(vk)) => map[n as usize] = vk,
                _ => bad.push(format!("{note} = \"{key}\"")),
            }
        }
        (map, bad)
    }
}

/// Путь к config.toml рядом с программой.
pub fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("config.toml")
}

/// Загружает настройки. Второе значение — предупреждение для показа в окне.
pub fn load(path: &Path) -> (Config, Option<String>) {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => {
            // Файла нет — создаём из шаблона.
            let _ = fs::write(path, TEMPLATE);
            return (Config::default(), None);
        }
    };
    // Блокнот иногда сохраняет файл с невидимой меткой BOM в начале — убираем её.
    let text = text.trim_start_matches('\u{feff}');
    match toml::from_str::<Config>(text) {
        Ok(mut cfg) => {
            cfg.sanitize();
            (cfg, None)
        }
        Err(e) => {
            let broken = path.with_file_name("config.broken.toml");
            let _ = fs::rename(path, &broken);
            let _ = fs::write(path, TEMPLATE);
            (
                Config::default(),
                Some(format!(
                    "Ошибка в config.toml — файл сохранён как config.broken.toml, настройки сброшены.\n{e}"
                )),
            )
        }
    }
}

/// Сохраняет настройки, не трогая комментарии в файле.
pub fn save(path: &Path, cfg: &Config) -> Result<()> {
    let text = fs::read_to_string(path).unwrap_or_else(|_| TEMPLATE.to_string());
    let mut doc: DocumentMut = text
        .trim_start_matches('\u{feff}')
        .parse()
        .or_else(|_| TEMPLATE.parse())
        .context("не удалось разобрать config.toml")?;

    let round2 = |v: f32| ((v as f64) * 100.0).round() / 100.0;
    let catchup = match cfg.lag.catchup {
        Catchup::Speedup => "speedup",
        Catchup::Skip => "skip",
    };
    let hk_mode = match cfg.hotkey.mode {
        HotkeyMode::Hold => "hold",
        HotkeyMode::Toggle => "toggle",
    };

    let a = &cfg.audio;
    set(&mut doc, "audio", "input_device", a.input_device.as_str().into());
    set(&mut doc, "audio", "output_device", a.output_device.as_str().into());
    set(&mut doc, "audio", "input_gain_db", round2(a.input_gain_db).into());
    set(&mut doc, "audio", "monitor", a.monitor.into());
    set(&mut doc, "audio", "monitor_device", a.monitor_device.as_str().into());
    set(&mut doc, "audio", "monitor_volume", (a.monitor_volume as i64).into());
    set(&mut doc, "audio", "buffer_ms", (a.buffer_ms as i64).into());

    let l = &cfg.lag;
    set(&mut doc, "lag", "mode", l.mode.config_name().into());
    set(&mut doc, "lag", "intensity", (l.intensity as i64).into());
    set(&mut doc, "lag", "catchup", catchup.into());
    set(&mut doc, "lag", "catchup_speed", round2(l.catchup_speed).into());
    set(&mut doc, "lag", "bitrate", (l.bitrate as i64).into());

    set(&mut doc, "hotkey", "key", cfg.hotkey.key.as_str().into());
    set(&mut doc, "hotkey", "mode", hk_mode.into());

    let p = &cfg.piano;
    set(&mut doc, "piano", "enabled", p.enabled.into());
    set(&mut doc, "piano", "soundfont", p.soundfont.as_str().into());
    set(&mut doc, "piano", "instrument", (p.instrument as i64).into());
    set(&mut doc, "piano", "volume", (p.volume as i64).into());
    set(&mut doc, "piano", "monitor", p.monitor.into());
    set(&mut doc, "piano", "through_lag", p.through_lag.into());
    set(&mut doc, "piano", "transpose", (p.transpose as i64).into());

    let m = &cfg.midi;
    set(&mut doc, "midi", "device", m.device.as_str().into());
    set(&mut doc, "midi", "send_keys", m.send_keys.into());
    set(&mut doc, "midi", "pedal_key", m.pedal_key.as_str().into());

    // Привязки переписываем целиком, по возрастанию нот.
    let root = doc.as_table_mut();
    if !root.contains_key("keymap") {
        root.insert("keymap", toml_edit::table());
    }
    if let Some(tbl) = root.get_mut("keymap").and_then(Item::as_table_mut) {
        tbl.clear();
        let mut entries: Vec<(&String, &String)> = cfg.keymap.iter().collect();
        entries.sort_by_key(|(n, _)| notes::parse_note(n).map(i32::from).unwrap_or(1000));
        for (note, key) in entries {
            tbl.insert(note, toml_edit::value(key.as_str()));
        }
    }

    fs::write(path, doc.to_string()).with_context(|| format!("не удалось записать {}", path.display()))
}

/// Записывает значение, сохраняя комментарии и оформление вокруг него.
fn set(doc: &mut DocumentMut, table: &str, key: &str, mut value: Value) {
    let root = doc.as_table_mut();
    if !root.contains_key(table) {
        root.insert(table, toml_edit::table());
    }
    let Some(tbl) = root.get_mut(table).and_then(Item::as_table_like_mut) else {
        return;
    };
    match tbl.get_mut(key) {
        Some(item) => {
            if let Some(old) = item.as_value() {
                *value.decor_mut() = old.decor().clone();
            }
            *item = Item::Value(value);
        }
        None => {
            tbl.insert(key, Item::Value(value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_matches_defaults() {
        let parsed: Config = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn save_keeps_comments_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!("mic-editor-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, TEMPLATE).unwrap();

        let mut cfg = Config::default();
        cfg.audio.input_device = "Микрофон (USB)".into();
        cfg.audio.monitor = true;
        cfg.audio.input_gain_db = 3.3;
        cfg.lag.mode = LagMode::Stutter;
        cfg.lag.intensity = 85;
        cfg.lag.catchup = Catchup::Skip;
        cfg.lag.catchup_speed = 2.1;
        cfg.hotkey.key = "Mouse4".into();
        cfg.hotkey.mode = HotkeyMode::Toggle;
        cfg.piano.instrument = 4;
        cfg.piano.transpose = -12;
        cfg.midi.send_keys = true;
        cfg.midi.pedal_key = "LShift".into();
        cfg.keymap.insert("E4".into(), "W".into());
        cfg.keymap.insert("C#4".into(), "Mouse2".into());
        cfg.keymap.insert("C4".into(), "Q".into());
        save(&path, &cfg).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Режим лага:"), "комментарии пропали:\n{text}");
        assert!(text.contains("# Привязки «нота = клавиша»"), "комментарий keymap пропал:\n{text}");
        assert!(text.contains("mode = \"stutter\""), "{text}");
        assert!(text.contains("catchup_speed = 2.1\n"), "{text}");
        assert!(text.contains("[keymap]\nC4 = \"Q\"\n\"C#4\" = \"Mouse2\"\nE4 = \"W\""), "{text}");
        let (map, bad) = cfg.keymap_table();
        assert!(bad.is_empty());
        assert_eq!(map[60], 0x51);
        assert_eq!(map[61], 0x02);
        let (loaded, warn) = load(&path);
        assert!(warn.is_none());
        assert_eq!(loaded, cfg);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bom_is_ignored() {
        let dir = std::env::temp_dir().join(format!("mic-editor-test-bom-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, format!("\u{feff}{TEMPLATE}")).unwrap();
        let (cfg, warn) = load(&path);
        assert!(warn.is_none(), "{warn:?}");
        assert_eq!(cfg, Config::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_file_is_replaced() {
        let dir = std::env::temp_dir().join(format!("mic-editor-test-broken-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "[lag]\nmode = \"nonsense\"\n").unwrap();
        let (cfg, warn) = load(&path);
        assert!(warn.is_some());
        assert_eq!(cfg, Config::default());
        assert!(dir.join("config.broken.toml").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
