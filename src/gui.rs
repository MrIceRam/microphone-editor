//! Окно программы (egui).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Vec2};

use crate::config::{self, Catchup, Config, HotkeyMode};
use crate::engine::{self, AudioSettings, Cmd};
use crate::keys;
use crate::lag::LagMode;
use crate::midi;
use crate::notes;
use crate::piano;
use crate::state::{CAPTURE_CANCELLED, Shared};

const RED: Color32 = Color32::from_rgb(220, 60, 60);
const ORANGE: Color32 = Color32::from_rgb(230, 150, 40);
const GREEN: Color32 = Color32::from_rgb(70, 180, 90);
const KEY_DOWN: Color32 = Color32::from_rgb(90, 160, 240);

/// Для чего сейчас ловится клавиша клавиатуры.
#[derive(Clone, Copy, PartialEq)]
enum CapturePurpose {
    Hotkey,
    /// Клавиша для ноты.
    Keymap(u8),
    Pedal,
}

/// Шаги добавления привязки «нота → клавиша».
#[derive(Clone, Copy, PartialEq)]
enum Learn {
    Idle,
    WaitNote,
    WaitKey(u8),
}

pub struct App {
    shared: Arc<Shared>,
    engine: Sender<Cmd>,
    cfg: Config,
    cfg_path: PathBuf,
    config_warning: Option<String>,
    save_error: Option<String>,
    inputs: Vec<String>,
    outputs: Vec<String>,
    soundfonts: Vec<(String, PathBuf)>,
    /// Когда последний раз что-то поменяли (сохраняем с небольшой задержкой).
    dirty_since: Option<Instant>,
    /// С какими настройками устройств сейчас работает движок.
    last_audio: AudioSettings,
    capture: CapturePurpose,
    learn: Learn,
    /// Нота, зажатая мышкой на экранном пианино.
    screen_note: Option<u8>,
    /// Октава первой «до» на экранном пианино.
    screen_octave: i32,
}

/// Переносит настройки из конфига в общее состояние (кроме устройств).
pub fn apply_to_shared(cfg: &Config, shared: &Shared) {
    shared.lag_mode.store(cfg.lag.mode as u8, Ordering::Relaxed);
    shared.intensity.set(cfg.lag.intensity as f32 / 100.0);
    shared.catchup_skip.store(cfg.lag.catchup == Catchup::Skip, Ordering::Relaxed);
    shared.catchup_speed.set(cfg.lag.catchup_speed);
    shared.bitrate.store(cfg.lag.bitrate, Ordering::Relaxed);
    shared.input_gain.set(10f32.powf(cfg.audio.input_gain_db / 20.0));
    shared.monitor_volume.set(cfg.audio.monitor_volume as f32 / 100.0);
    shared.hotkey_vk.store(keys::parse_key(&cfg.hotkey.key).unwrap_or(0), Ordering::Relaxed);
    let toggle = cfg.hotkey.mode == HotkeyMode::Toggle;
    if shared.hotkey_toggle.swap(toggle, Ordering::Relaxed) != toggle {
        shared.lag_held.store(false, Ordering::Relaxed);
    }

    shared.piano_enabled.store(cfg.piano.enabled, Ordering::Relaxed);
    shared.piano_volume.set(cfg.piano.volume as f32 / 100.0);
    shared.piano_program.store(cfg.piano.instrument, Ordering::Relaxed);
    shared.piano_through_lag.store(cfg.piano.through_lag, Ordering::Relaxed);
    shared.piano_transpose.store(cfg.piano.transpose, Ordering::Relaxed);
    shared.monitor_voice.store(cfg.audio.monitor, Ordering::Relaxed);
    shared.monitor_piano.store(cfg.piano.enabled && cfg.piano.monitor, Ordering::Relaxed);

    *shared.midi_device.lock().unwrap() = cfg.midi.device.clone();
    let (map, _) = cfg.keymap_table();
    let mut km = shared.keymapper.lock().unwrap();
    km.map = map;
    km.pedal_vk = keys::parse_key(&cfg.midi.pedal_key).unwrap_or(0);
    km.set_enabled(cfg.midi.send_keys);
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shared: Arc<Shared>,
        engine: Sender<Cmd>,
        cfg: Config,
        cfg_path: PathBuf,
        config_warning: Option<String>,
    ) -> Self {
        cc.egui_ctx.global_style_mut(|style| {
            for font in style.text_styles.values_mut() {
                font.size *= 1.1;
            }
            style.spacing.item_spacing.y = 6.0;
        });
        let (inputs, outputs) = engine::list_devices();
        let last_audio = cfg.audio_settings();
        let mut app = Self {
            shared,
            engine,
            cfg,
            cfg_path,
            config_warning,
            save_error: None,
            inputs,
            outputs,
            soundfonts: piano::list_soundfonts(),
            dirty_since: None,
            last_audio,
            capture: CapturePurpose::Hotkey,
            learn: Learn::Idle,
            screen_note: None,
            screen_octave: 4,
        };
        app.check_keymap();
        app
    }

    fn mark_dirty(&mut self) {
        apply_to_shared(&self.cfg, &self.shared);
        self.dirty_since = Some(Instant::now());
    }

    fn restart_audio(&mut self) {
        self.last_audio = self.cfg.audio_settings();
        let _ = self.engine.send(Cmd::Restart(self.last_audio.clone()));
    }

    fn save_now(&mut self) {
        self.dirty_since = None;
        self.save_error = config::save(&self.cfg_path, &self.cfg)
            .err()
            .map(|e| format!("Не удалось сохранить настройки: {e:#}"));
    }

    fn reload_config(&mut self) {
        let (cfg, warn) = config::load(&self.cfg_path);
        self.cfg = cfg;
        self.config_warning = warn;
        apply_to_shared(&self.cfg, &self.shared);
        self.check_keymap();
        if self.cfg.audio_settings() != self.last_audio {
            self.restart_audio();
        }
    }

    /// Предупреждение о непонятных строках в [keymap].
    fn check_keymap(&mut self) {
        let (_, bad) = self.cfg.keymap_table();
        if !bad.is_empty() {
            let msg = format!("Не понял привязки в [keymap]: {}", bad.join(", "));
            self.config_warning = Some(match self.config_warning.take() {
                Some(w) => format!("{w}\n{msg}"),
                None => msg,
            });
        }
    }

    fn start_capture(&mut self, purpose: CapturePurpose) {
        self.capture = purpose;
        self.shared.captured_vk.store(0, Ordering::Relaxed);
        self.shared.capturing.store(true, Ordering::Relaxed);
    }

    fn stop_capture(&mut self) {
        self.shared.capturing.store(false, Ordering::Relaxed);
        self.shared.captured_vk.store(0, Ordering::Relaxed);
    }

    fn capturing(&self, purpose: CapturePurpose) -> bool {
        self.capture == purpose && self.shared.capturing.load(Ordering::Relaxed)
    }

    fn set_keymap(&mut self, note: u8, key: String) {
        // Убираем старые записи этой ноты (в т.ч. написанные иначе, например Db4).
        self.cfg.keymap.retain(|n, _| notes::parse_note(n) != Some(note));
        self.cfg.keymap.insert(notes::note_name(note), key);
        self.mark_dirty();
    }

    /// Разбирает клавишу, пойманную потоком горячих клавиш.
    fn handle_capture(&mut self) {
        let captured = self.shared.captured_vk.swap(0, Ordering::Relaxed);
        if captured == 0 {
            return;
        }
        if captured == CAPTURE_CANCELLED {
            if matches!(self.capture, CapturePurpose::Keymap(_)) {
                self.learn = Learn::Idle;
            }
            return;
        }
        let name = keys::key_name(captured);
        match self.capture {
            CapturePurpose::Hotkey => {
                self.cfg.hotkey.key = name;
                self.mark_dirty();
            }
            CapturePurpose::Keymap(note) => {
                self.set_keymap(note, name);
                self.learn = Learn::Idle;
            }
            CapturePurpose::Pedal => {
                self.cfg.midi.pedal_key = name;
                self.mark_dirty();
            }
        }
    }

    // --- Блоки интерфейса -------------------------------------------------

    fn lag_block(&mut self, ui: &mut egui::Ui) {
        let active = self.shared.lag_active();
        let wet = self.shared.lag_wet.get();
        let (text, fill) = if active {
            ("ЛАГ ВКЛЮЧЁН", RED)
        } else {
            ("ЛАГ ВЫКЛЮЧЕН", Color32::from_gray(60))
        };
        let button = egui::Button::new(RichText::new(text).size(26.0).strong().color(Color32::WHITE))
            .fill(fill)
            .min_size(egui::vec2(ui.available_width(), 56.0));
        if ui.add(button).on_hover_text("Нажми, чтобы включить/выключить лаг").clicked() {
            self.shared.lag_latched.fetch_xor(true, Ordering::Relaxed);
        }
        if wet > 0.0 && wet < 1.0 {
            ui.ctx().request_repaint();
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Горячая клавиша:");
            if self.capturing(CapturePurpose::Hotkey) {
                ui.label(RichText::new("нажми любую клавишу… (Esc — отмена)").color(ORANGE));
            } else {
                let name = if self.cfg.hotkey.key.trim().is_empty() {
                    "нет".to_string()
                } else {
                    self.cfg.hotkey.key.clone()
                };
                ui.label(RichText::new(name).strong().monospace());
                if ui.button("Назначить…").clicked() {
                    self.start_capture(CapturePurpose::Hotkey);
                }
                if !self.cfg.hotkey.key.is_empty() && ui.button("Убрать").clicked() {
                    self.cfg.hotkey.key.clear();
                    self.mark_dirty();
                }
            }
        });

        ui.horizontal(|ui| {
            let mut mode = self.cfg.hotkey.mode;
            ui.radio_value(&mut mode, HotkeyMode::Hold, "Удерживать");
            ui.radio_value(&mut mode, HotkeyMode::Toggle, "Нажал — вкл, ещё раз — выкл");
            if mode != self.cfg.hotkey.mode {
                self.cfg.hotkey.mode = mode;
                self.shared.lag_latched.store(false, Ordering::Relaxed);
                self.mark_dirty();
            }
        });

        ui.add_space(6.0);
        ui.label(RichText::new("Режим").strong());
        let mut mode = self.cfg.lag.mode;
        for m in LagMode::ALL {
            ui.horizontal(|ui| {
                ui.radio_value(&mut mode, m, m.label());
                ui.weak(m.description());
            });
        }
        if mode != self.cfg.lag.mode {
            self.cfg.lag.mode = mode;
            self.mark_dirty();
        }

        ui.add_space(4.0);
        let mut intensity = self.cfg.lag.intensity;
        ui.add(egui::Slider::new(&mut intensity, 0..=100).text("сила").suffix("%"));
        if intensity != self.cfg.lag.intensity {
            self.cfg.lag.intensity = intensity;
            self.mark_dirty();
        }

        if matches!(self.cfg.lag.mode, LagMode::Spikes | LagMode::Mix) {
            ui.horizontal(|ui| {
                ui.label("После скачка пинга:");
                let mut c = self.cfg.lag.catchup;
                ui.radio_value(&mut c, Catchup::Speedup, "догонять ускоренно");
                ui.radio_value(&mut c, Catchup::Skip, "перескакивать");
                if c != self.cfg.lag.catchup {
                    self.cfg.lag.catchup = c;
                    self.mark_dirty();
                }
            });
            if self.cfg.lag.catchup == Catchup::Speedup {
                let mut speed = self.cfg.lag.catchup_speed;
                ui.add(egui::Slider::new(&mut speed, 1.1..=3.0).text("скорость догона").suffix("×").fixed_decimals(1));
                if (speed - self.cfg.lag.catchup_speed).abs() > f32::EPSILON {
                    self.cfg.lag.catchup_speed = speed;
                    self.mark_dirty();
                }
            }
        }
    }

    fn devices_block(&mut self, ui: &mut egui::Ui) {
        let mut restart = false;

        egui::Grid::new("devices").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
            ui.label("Микрофон:");
            let shown = if self.cfg.audio.input_device.is_empty() {
                "По умолчанию (Windows)".to_string()
            } else {
                self.cfg.audio.input_device.clone()
            };
            egui::ComboBox::from_id_salt("input")
                .width(330.0)
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    let mut v = self.cfg.audio.input_device.clone();
                    ui.selectable_value(&mut v, String::new(), "По умолчанию (Windows)");
                    for name in &self.inputs {
                        ui.selectable_value(&mut v, name.clone(), name);
                    }
                    if v != self.cfg.audio.input_device {
                        self.cfg.audio.input_device = v;
                        restart = true;
                    }
                });
            ui.end_row();

            ui.label("Выход (в Discord):");
            egui::ComboBox::from_id_salt("output")
                .width(330.0)
                .selected_text(if self.cfg.audio.output_device.is_empty() {
                    "не выбран".to_string()
                } else {
                    self.cfg.audio.output_device.clone()
                })
                .show_ui(ui, |ui| {
                    let mut v = self.cfg.audio.output_device.clone();
                    for name in &self.outputs {
                        ui.selectable_value(&mut v, name.clone(), name);
                    }
                    if v != self.cfg.audio.output_device {
                        self.cfg.audio.output_device = v;
                        restart = true;
                    }
                });
            ui.end_row();

            ui.label("Усиление:");
            let mut gain = self.cfg.audio.input_gain_db;
            ui.add(egui::Slider::new(&mut gain, -24.0..=24.0).suffix(" дБ").fixed_decimals(1));
            if (gain - self.cfg.audio.input_gain_db).abs() > f32::EPSILON {
                self.cfg.audio.input_gain_db = gain;
                self.mark_dirty();
            }
            ui.end_row();

            let mut monitor = self.cfg.audio.monitor;
            ui.checkbox(&mut monitor, "Слышать себя:");
            if monitor != self.cfg.audio.monitor {
                self.cfg.audio.monitor = monitor;
                restart = true;
            }
            let headphones_used = self.cfg.audio.monitor || (self.cfg.piano.enabled && self.cfg.piano.monitor);
            ui.add_enabled_ui(headphones_used, |ui| {
                egui::ComboBox::from_id_salt("monitor")
                    .width(330.0)
                    .selected_text(if self.cfg.audio.monitor_device.is_empty() {
                        "По умолчанию (Windows)".to_string()
                    } else {
                        self.cfg.audio.monitor_device.clone()
                    })
                    .show_ui(ui, |ui| {
                        let mut v = self.cfg.audio.monitor_device.clone();
                        ui.selectable_value(&mut v, String::new(), "По умолчанию (Windows)");
                        for name in &self.outputs {
                            ui.selectable_value(&mut v, name.clone(), name);
                        }
                        if v != self.cfg.audio.monitor_device {
                            self.cfg.audio.monitor_device = v;
                            restart = true;
                        }
                    });
            });
            ui.end_row();

            ui.label("Громкость в наушниках:");
            let mut vol = self.cfg.audio.monitor_volume;
            ui.add_enabled(headphones_used, egui::Slider::new(&mut vol, 0..=100).suffix("%"));
            if vol != self.cfg.audio.monitor_volume {
                self.cfg.audio.monitor_volume = vol;
                self.mark_dirty();
            }
            ui.end_row();
        });

        ui.horizontal(|ui| {
            if ui.button("Обновить список устройств").clicked() {
                let (i, o) = engine::list_devices();
                self.inputs = i;
                self.outputs = o;
                restart = true;
            }
        });

        if restart {
            self.mark_dirty();
            self.restart_audio();
        }

        let out_lc = self.cfg.audio.output_device.to_lowercase();
        if !out_lc.is_empty() && !out_lc.contains("cable") {
            ui.colored_label(
                ORANGE,
                "⚠ Выход — не виртуальный кабель. Discord услышит только то, что идёт в «CABLE Input».",
            );
        }
    }

    fn piano_block(&mut self, ui: &mut egui::Ui) {
        let midi = self.shared.midi_status.lock().unwrap().clone();
        ui.horizontal(|ui| {
            ui.label("MIDI-клавиатура:");
            match &midi.connected {
                Some(n) => ui.colored_label(GREEN, format!("● {n}")),
                None => ui.colored_label(ORANGE, "не найдена — подключи, подхватится сама"),
            };
        });
        if midi.ports.len() > 1 || !self.cfg.midi.device.is_empty() {
            ui.horizontal(|ui| {
                ui.label("Устройство:");
                egui::ComboBox::from_id_salt("midi")
                    .width(300.0)
                    .selected_text(if self.cfg.midi.device.is_empty() {
                        "Первое найденное".to_string()
                    } else {
                        self.cfg.midi.device.clone()
                    })
                    .show_ui(ui, |ui| {
                        let mut v = self.cfg.midi.device.clone();
                        ui.selectable_value(&mut v, String::new(), "Первое найденное");
                        for p in &midi.ports {
                            ui.selectable_value(&mut v, p.clone(), p);
                        }
                        if v != self.cfg.midi.device {
                            self.cfg.midi.device = v;
                            self.mark_dirty();
                        }
                    });
            });
        }
        if let Some(e) = &midi.error {
            ui.colored_label(ORANGE, format!("⚠ {e}"));
        }

        let mut enabled = self.cfg.piano.enabled;
        ui.checkbox(&mut enabled, "Звук пианино в микрофон");
        if enabled != self.cfg.piano.enabled {
            self.cfg.piano.enabled = enabled;
            self.mark_dirty();
        }

        ui.add_enabled_ui(self.cfg.piano.enabled, |ui| {
            egui::Grid::new("piano").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                ui.label("Инструмент:");
                egui::ComboBox::from_id_salt("instrument")
                    .width(240.0)
                    .selected_text(piano::instrument_name(self.cfg.piano.instrument))
                    .show_ui(ui, |ui| {
                        let mut v = self.cfg.piano.instrument;
                        for (p, name) in piano::INSTRUMENTS {
                            ui.selectable_value(&mut v, *p, *name);
                        }
                        if v != self.cfg.piano.instrument {
                            self.cfg.piano.instrument = v;
                            self.mark_dirty();
                        }
                    });
                ui.end_row();

                ui.label("SoundFont:");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("soundfont")
                        .width(240.0)
                        .selected_text(if self.cfg.piano.soundfont.is_empty() {
                            "Первый найденный".to_string()
                        } else {
                            self.cfg.piano.soundfont.clone()
                        })
                        .show_ui(ui, |ui| {
                            let mut v = self.cfg.piano.soundfont.clone();
                            ui.selectable_value(&mut v, String::new(), "Первый найденный");
                            for (name, _) in &self.soundfonts {
                                ui.selectable_value(&mut v, name.clone(), name);
                            }
                            if v != self.cfg.piano.soundfont {
                                self.cfg.piano.soundfont = v;
                                self.mark_dirty();
                            }
                        });
                    if ui.button("Папка").on_hover_text("Открыть папку с .sf2 — можно добавить свои").clicked() {
                        let dir = piano::soundfont_folder();
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                    }
                    if ui.button("⟳").on_hover_text("Обновить список .sf2").clicked() {
                        self.soundfonts = piano::list_soundfonts();
                    }
                });
                ui.end_row();

                ui.label("Громкость:");
                let mut vol = self.cfg.piano.volume;
                ui.add(egui::Slider::new(&mut vol, 0..=100).suffix("%"));
                if vol != self.cfg.piano.volume {
                    self.cfg.piano.volume = vol;
                    self.mark_dirty();
                }
                ui.end_row();

                ui.label("Сдвиг высоты:");
                let mut tr = self.cfg.piano.transpose;
                ui.add(egui::Slider::new(&mut tr, -24..=24).suffix(" полутонов"));
                if tr != self.cfg.piano.transpose {
                    self.cfg.piano.transpose = tr;
                    self.mark_dirty();
                }
                ui.end_row();
            });

            let mut mon = self.cfg.piano.monitor;
            ui.checkbox(&mut mon, "Слышать пианино в наушниках");
            if mon != self.cfg.piano.monitor {
                self.cfg.piano.monitor = mon;
                self.mark_dirty();
            }
            let mut tl = self.cfg.piano.through_lag;
            ui.checkbox(&mut tl, "Пианино лагает вместе с голосом");
            if tl != self.cfg.piano.through_lag {
                self.cfg.piano.through_lag = tl;
                self.mark_dirty();
            }
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Экранное пианино:");
            if ui.small_button("◀").clicked() && self.screen_octave > 0 {
                self.screen_octave -= 1;
            }
            let base = ((self.screen_octave + 1) * 12) as u8;
            ui.monospace(format!("{}–{}", notes::note_name(base), notes::note_name(base + 24)));
            if ui.small_button("▶").clicked() && self.screen_octave < 7 {
                self.screen_octave += 1;
            }
        });
        self.draw_piano(ui);

        let last = self.shared.last_note.load(Ordering::Relaxed);
        ui.horizontal(|ui| {
            if last >= 0 {
                let n = last as u8;
                ui.weak(format!(
                    "Последняя нота: {} ({}), сила {}",
                    notes::note_name(n),
                    notes::note_name_ru(n),
                    self.shared.last_velocity.load(Ordering::Relaxed)
                ));
            } else {
                ui.weak("Нажми клавишу на пианино или мышкой на экранном пианино");
            }
            if self.shared.pedal_down.load(Ordering::Relaxed) {
                ui.colored_label(KEY_DOWN, "педаль");
            }
        });
    }

    /// Рисует экранное пианино на 2 октавы и обрабатывает клики мышкой.
    fn draw_piano(&mut self, ui: &mut egui::Ui) {
        let base = ((self.screen_octave + 1) * 12) as u8;
        let last = base + 24;
        let width = ui.available_width().min(560.0);
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 96.0), Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        let whites: Vec<u8> = (base..=last).filter(|&n| !notes::is_black(n)).collect();
        let ww = rect.width() / whites.len() as f32;
        let white_rects: Vec<(u8, Rect)> = whites
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let r = Rect::from_min_size(Pos2::new(rect.left() + i as f32 * ww, rect.top()), Vec2::new(ww, rect.height()));
                (n, r)
            })
            .collect();
        let black_rects: Vec<(u8, Rect)> = (base..=last)
            .filter(|&n| notes::is_black(n))
            .filter_map(|n| {
                let i = whites.iter().position(|&w| w == n - 1)?;
                let x = rect.left() + (i + 1) as f32 * ww;
                let r = Rect::from_center_size(
                    Pos2::new(x, rect.top() + rect.height() * 0.31),
                    Vec2::new(ww * 0.6, rect.height() * 0.62),
                );
                Some((n, r))
            })
            .collect();

        // Мышь: какая клавиша под курсором (чёрные лежат поверх белых).
        let pointer_note = if response.is_pointer_button_down_on() {
            response.interact_pointer_pos().and_then(|p| {
                black_rects
                    .iter()
                    .chain(white_rects.iter())
                    .find(|(_, r)| r.contains(p))
                    .map(|(n, _)| *n)
            })
        } else {
            None
        };
        if pointer_note != self.screen_note {
            if let Some(old) = self.screen_note {
                midi::handle_message(&self.shared, &[0x80, old, 0], false);
            }
            if let Some(n) = pointer_note {
                midi::handle_message(&self.shared, &[0x90, n, 100], false);
            }
            self.screen_note = pointer_note;
        }

        painter.rect_filled(rect, 3.0, Color32::from_gray(30));
        for (n, r) in &white_rects {
            let fill = if self.shared.note_is_down(*n) { KEY_DOWN } else { Color32::from_gray(235) };
            painter.rect_filled(r.shrink(1.0), 3.0, fill);
            if n % 12 == 0 {
                painter.text(
                    Pos2::new(r.center().x, r.bottom() - 4.0),
                    Align2::CENTER_BOTTOM,
                    notes::note_name(*n),
                    FontId::proportional(11.0),
                    Color32::from_gray(90),
                );
            }
        }
        for (n, r) in &black_rects {
            let fill = if self.shared.note_is_down(*n) { KEY_DOWN } else { Color32::from_gray(25) };
            painter.rect_filled(*r, 2.0, fill);
        }
    }

    fn keymap_block(&mut self, ui: &mut egui::Ui) {
        let mut send = self.cfg.midi.send_keys;
        ui.checkbox(&mut send, RichText::new("Нажимать клавиши по нотам (для игр)").strong());
        if send != self.cfg.midi.send_keys {
            self.cfg.midi.send_keys = send;
            self.mark_dirty();
        }
        ui.weak("Нажимает только настоящее пианино, экранное — нет. Одна нота = одна клавиша.");

        // Педаль.
        ui.horizontal(|ui| {
            ui.label("Правая педаль →");
            if self.capturing(CapturePurpose::Pedal) {
                ui.label(RichText::new("нажми клавишу… (Esc — отмена)").color(ORANGE));
            } else {
                let name = if self.cfg.midi.pedal_key.is_empty() { "нет".to_string() } else { self.cfg.midi.pedal_key.clone() };
                ui.label(RichText::new(name).strong().monospace());
                if ui.button("Назначить…").clicked() {
                    self.start_capture(CapturePurpose::Pedal);
                }
                if !self.cfg.midi.pedal_key.is_empty() && ui.button("Убрать").clicked() {
                    self.cfg.midi.pedal_key.clear();
                    self.mark_dirty();
                }
            }
        });

        // Список привязок.
        let mut entries: Vec<(u8, String, String)> = self
            .cfg
            .keymap
            .iter()
            .filter_map(|(n, k)| notes::parse_note(n).map(|nn| (nn, n.clone(), k.clone())))
            .collect();
        entries.sort_by_key(|e| e.0);
        let mut remove = None;
        if entries.is_empty() {
            ui.weak("Привязок пока нет.");
        } else {
            egui::Grid::new("keymap").num_columns(3).spacing([16.0, 4.0]).striped(true).show(ui, |ui| {
                for (n, raw, key) in &entries {
                    let down = self.shared.note_is_down(*n);
                    let text = format!("{} ({})", notes::note_name(*n), notes::note_name_ru(*n));
                    if down {
                        ui.colored_label(KEY_DOWN, text);
                    } else {
                        ui.label(text);
                    }
                    ui.label(RichText::new(format!("→ {key}")).strong().monospace());
                    if ui.small_button("✖").on_hover_text("Удалить привязку").clicked() {
                        remove = Some(raw.clone());
                    }
                    ui.end_row();
                }
            });
        }
        if let Some(r) = remove {
            self.cfg.keymap.remove(&r);
            self.mark_dirty();
        }

        // Добавление привязки.
        match self.learn {
            Learn::Idle => {
                if ui.button("➕ Добавить привязку").clicked() {
                    self.shared.learned_note.store(-1, Ordering::Relaxed);
                    self.shared.learn_note.store(true, Ordering::Relaxed);
                    self.learn = Learn::WaitNote;
                }
            }
            Learn::WaitNote => {
                let n = self.shared.learned_note.swap(-1, Ordering::Relaxed);
                if n >= 0 {
                    self.learn = Learn::WaitKey(n as u8);
                    self.start_capture(CapturePurpose::Keymap(n as u8));
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Нажми ноту на пианино (или на экранном)…").color(ORANGE));
                    if ui.button("Отмена").clicked() {
                        self.shared.learn_note.store(false, Ordering::Relaxed);
                        self.learn = Learn::Idle;
                    }
                });
                ui.ctx().request_repaint_after(Duration::from_millis(30));
            }
            Learn::WaitKey(note) => {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{} → нажми клавишу на клавиатуре…", notes::note_name(note))).color(ORANGE),
                    );
                    if ui.button("ЛКМ").on_hover_text("Левая кнопка мыши").clicked() {
                        self.stop_capture();
                        self.set_keymap(note, "Mouse1".into());
                        self.learn = Learn::Idle;
                    }
                    if ui.button("ПКМ").on_hover_text("Правая кнопка мыши").clicked() {
                        self.stop_capture();
                        self.set_keymap(note, "Mouse2".into());
                        self.learn = Learn::Idle;
                    }
                    if ui.button("Отмена").clicked() {
                        self.stop_capture();
                        self.learn = Learn::Idle;
                    }
                });
                ui.ctx().request_repaint_after(Duration::from_millis(30));
            }
        }
    }

    fn meters_block(&mut self, ui: &mut egui::Ui) {
        let to_bar = |level: f32| {
            let db = 20.0 * level.max(1e-6).log10();
            ((db + 60.0) / 60.0).clamp(0.0, 1.0)
        };
        let input = self.shared.in_level.get();
        let output = self.shared.out_level.get();
        egui::Grid::new("meters").num_columns(2).show(ui, |ui| {
            ui.label("Микрофон:");
            ui.add(
                egui::ProgressBar::new(to_bar(input))
                    .desired_width(330.0)
                    .fill(if input > 0.98 { RED } else { GREEN }),
            );
            ui.end_row();
            ui.label("В Discord:");
            ui.add(
                egui::ProgressBar::new(to_bar(output))
                    .desired_width(330.0)
                    .fill(if self.shared.lag_active() { RED } else { GREEN }),
            );
            ui.end_row();
        });

        // Сбои звука (обрывы буфера) с момента запуска — если растёт, звук рвётся.
        let glitches: u64 = [&self.shared.input_stats, &self.shared.main_stats, &self.shared.monitor_stats]
            .iter()
            .map(|s| s.underruns_total.load(Ordering::Relaxed))
            .sum();
        if glitches > 0 {
            ui.colored_label(ORANGE, format!("Сбоев звука с запуска: {glitches}"))
                .on_hover_text("Если число быстро растёт — попробуй увеличить buffer_ms в файле настроек.");
        } else {
            ui.weak("Сбоев звука нет");
        }
    }

    fn status_block(&mut self, ui: &mut egui::Ui) {
        let st = self.shared.status();
        if let Some(err) = &st.error {
            ui.colored_label(RED, format!("✖ {err}"));
            ui.weak("Пробую снова каждые 2 секунды…");
        } else if st.running {
            ui.colored_label(GREEN, format!("● Работает. Микрофон: {}", st.input_name));
            if let Some(o) = &st.output_name {
                ui.weak(format!("Выход: {o}"));
            }
            if let Some(m) = &st.monitor_name {
                ui.weak(format!("Наушники: {m}"));
            }
            if let Some(sf) = &st.soundfont_name {
                ui.weak(format!("SoundFont: {sf}"));
            }
        } else {
            ui.weak("Запуск звука…");
        }
        for w in &st.warnings {
            ui.colored_label(ORANGE, format!("⚠ {w}"));
        }
        if let Some(w) = &self.config_warning {
            ui.colored_label(ORANGE, format!("⚠ {w}"));
        }
        if let Some(e) = &self.save_error {
            ui.colored_label(RED, e);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_capture();

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.lag_block(ui);
                ui.separator();
                ui.label(RichText::new("Устройства").strong());
                self.devices_block(ui);
                ui.separator();
                self.meters_block(ui);
                ui.separator();
                ui.label(RichText::new("Пианино").strong());
                self.piano_block(ui);
                ui.separator();
                ui.label(RichText::new("Клавиши для игр").strong());
                self.keymap_block(ui);
                ui.separator();
                self.status_block(ui);
                ui.separator();
                egui::CollapsingHeader::new("Как подключить к Discord").show(ui, |ui| {
                    ui.label("1. Установи VB-Audio Virtual Cable (vb-audio.com) и перезагрузи компьютер.");
                    ui.label("2. Здесь выбери выход «CABLE Input (VB-Audio Virtual Cable)».");
                    ui.label("3. В Discord: Настройки → Голос и видео → Устройство ввода: «CABLE Output».");
                    ui.label("4. Там же выключи шумоподавление Krisp (или поставь «Стандартное») —");
                    ui.label("    иначе Discord будет «чистить» звук лага и пианино.");
                    ui.label("5. Свой микрофон в Discord больше не выбирай — он идёт через эту программу.");
                });
                ui.horizontal(|ui| {
                    if ui.button("Открыть файл настроек").clicked() {
                        self.save_now();
                        let _ = std::process::Command::new("notepad").arg(&self.cfg_path).spawn();
                    }
                    if ui.button("Перечитать файл").clicked() {
                        self.reload_config();
                    }
                });
            });
        });

        // Настройки, требующие переоткрыть звук (устройства, SoundFont...).
        if self.cfg.audio_settings() != self.last_audio {
            self.restart_audio();
        }

        // Сохраняем через полсекунды после последнего изменения (чтобы не писать
        // файл на каждое движение ползунка).
        if let Some(t) = self.dirty_since {
            if t.elapsed() > Duration::from_millis(500) {
                self.save_now();
            } else {
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
        }
        // Индикаторы и подсветка нот обновляются ~30 раз в секунду.
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }

    fn on_exit(&mut self) {
        // Ни одна клавиша не должна остаться зажатой в игре.
        self.shared.keymapper.lock().unwrap().release_all();
        if self.dirty_since.is_some() {
            self.save_now();
        }
        let _ = self.engine.send(Cmd::Shutdown);
    }
}
