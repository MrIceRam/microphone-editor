//! Звук пианино: синтезатор SoundFont (.sf2).
//!
//! Файлы .sf2 лежат в папке `soundfonts` рядом с программой
//! (или в папке `soundfonts` проекта, если программа собрана из исходников).

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rustysynth::{SoundFont, Synthesizer, SynthesizerSettings};

use crate::lag::{FRAME, SAMPLE_RATE};

/// Синтезатор по умолчанию звучит тихо (аккорд ~0.15 от максимума) — поднимаем
/// до громкости, сравнимой с голосом.
const PIANO_GAIN: f32 = 3.0;

/// Инструменты General MIDI, которые показываются в окне: (номер программы, название).
pub const INSTRUMENTS: &[(u8, &str)] = &[
    (0, "Рояль"),
    (1, "Яркое пианино"),
    (4, "Электропиано"),
    (6, "Клавесин"),
    (11, "Вибрафон"),
    (16, "Электроорган"),
    (19, "Церковный орган"),
    (21, "Аккордеон"),
    (24, "Гитара (нейлон)"),
    (25, "Гитара (сталь)"),
    (29, "Перегруженная гитара"),
    (33, "Бас-гитара"),
    (40, "Скрипка"),
    (48, "Струнные"),
    (52, "Хор"),
    (56, "Труба"),
    (61, "Медные духовые"),
    (65, "Саксофон"),
    (73, "Флейта"),
    (80, "Синтезатор: квадрат"),
    (81, "Синтезатор: пила"),
    (88, "Синтезатор: пэд"),
];

pub fn instrument_name(program: u8) -> String {
    INSTRUMENTS
        .iter()
        .find(|(p, _)| *p == program)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| format!("Инструмент №{program}"))
}

/// Папки, где ищутся .sf2 (по порядку).
pub fn soundfont_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        dirs.push(exe_dir.join("soundfonts"));
    }
    // Папка проекта — когда программа запущена из target\release после сборки.
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("soundfonts");
    if !dirs.contains(&project) {
        dirs.push(project);
    }
    dirs
}

/// Все найденные .sf2: (имя файла, полный путь).
pub fn list_soundfonts() -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    for dir in soundfont_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let is_sf2 = path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("sf2"));
            if is_sf2 {
                let name = e.file_name().to_string_lossy().to_string();
                if !found.iter().any(|(n, _)| n == &name) {
                    found.push((name, path));
                }
            }
        }
    }
    found.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    found
}

/// Папка, которую открывать кнопкой «Открыть папку»: где уже есть .sf2, иначе рядом с программой.
pub fn soundfont_folder() -> PathBuf {
    let dirs = soundfont_dirs();
    for d in &dirs {
        if list_in(d) {
            return d.clone();
        }
    }
    dirs.into_iter().next().unwrap_or_else(|| PathBuf::from("soundfonts"))
}

fn list_in(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("sf2")))
        })
        .unwrap_or(false)
}

/// Находит файл SoundFont по настройке: полный путь, имя файла или пусто (= первый найденный).
pub fn resolve_soundfont(setting: &str) -> Option<PathBuf> {
    let setting = setting.trim();
    if !setting.is_empty() {
        let p = PathBuf::from(setting);
        if p.is_absolute() && p.exists() {
            return Some(p);
        }
    }
    let all = list_soundfonts();
    if setting.is_empty() {
        return all.into_iter().next().map(|(_, p)| p);
    }
    all.into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(setting))
        .map(|(_, p)| p)
}

pub fn load_soundfont(path: &Path) -> Result<Arc<SoundFont>> {
    let file = File::open(path).with_context(|| format!("не открыть {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let sf = SoundFont::new(&mut reader).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(Arc::new(sf))
}

/// Синтезатор, который играет ноты из MIDI-сообщений.
pub struct PianoSynth {
    synth: Synthesizer,
    left: Vec<f32>,
    right: Vec<f32>,
    program: i32,
    /// Какая нота синтезатора звучит для каждой нажатой клавиши (с учётом сдвига).
    sounding: [i16; 128],
}

impl PianoSynth {
    pub fn new(sf: &Arc<SoundFont>) -> Result<Self> {
        let mut settings = SynthesizerSettings::new(SAMPLE_RATE as i32);
        settings.maximum_polyphony = 64;
        settings.enable_reverb_and_chorus = true;
        let synth = Synthesizer::new(sf, &settings).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(Self {
            synth,
            left: vec![0.0; FRAME],
            right: vec![0.0; FRAME],
            program: -1,
            sounding: [-1; 128],
        })
    }

    pub fn set_program(&mut self, program: u8) {
        if self.program != program as i32 {
            self.program = program as i32;
            self.synth.process_midi_message(0, 0xC0, program as i32, 0);
        }
    }

    /// Обрабатывает сырое MIDI-сообщение. Все каналы сводятся на первый.
    pub fn handle(&mut self, msg: [u8; 3], transpose: i32) {
        let kind = msg[0] & 0xF0;
        let (d1, d2) = (msg[1] as i32 & 127, msg[2] as i32 & 127);
        match kind {
            0x90 if d2 > 0 => {
                let key = d1 as usize;
                if self.sounding[key] >= 0 {
                    self.synth.note_off(0, self.sounding[key] as i32);
                }
                let n = (d1 + transpose).clamp(0, 127);
                self.sounding[key] = n as i16;
                self.synth.note_on(0, n, d2);
            }
            0x80 | 0x90 => {
                let key = d1 as usize;
                if self.sounding[key] >= 0 {
                    self.synth.note_off(0, self.sounding[key] as i32);
                    self.sounding[key] = -1;
                }
            }
            // Смена инструмента с самого пианино игнорируется — инструмент выбирается в окне.
            0xC0 => {}
            0xA0 | 0xB0 | 0xD0 | 0xE0 => self.synth.process_midi_message(0, kind as i32, d1, d2),
            _ => {}
        }
    }

    pub fn all_notes_off(&mut self) {
        self.synth.note_off_all(false);
        self.sounding = [-1; 128];
    }

    /// Рендерит `out.len()` (не больше FRAME) сэмплов моно.
    pub fn render(&mut self, out: &mut [f32]) {
        let n = out.len().min(FRAME);
        self.synth.render(&mut self.left[..n], &mut self.right[..n]);
        for i in 0..n {
            out[i] = (self.left[i] + self.right[i]) * 0.5 * PIANO_GAIN;
        }
    }
}
