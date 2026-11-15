//! Окно программы (egui).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::config::{self, Catchup, Config, HotkeyMode};
use crate::engine::{self, Cmd};
use crate::keys;
use crate::lag::LagMode;
use crate::state::{CAPTURE_CANCELLED, Shared};

const RED: Color32 = Color32::from_rgb(220, 60, 60);
const ORANGE: Color32 = Color32::from_rgb(230, 150, 40);
const GREEN: Color32 = Color32::from_rgb(70, 180, 90);

pub struct App {
    shared: Arc<Shared>,
    engine: Sender<Cmd>,
    cfg: Config,
    cfg_path: PathBuf,
    config_warning: Option<String>,
    save_error: Option<String>,
    inputs: Vec<String>,
    outputs: Vec<String>,
    /// Когда последний раз что-то поменяли (сохраняем с небольшой задержкой).
    dirty_since: Option<Instant>,
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
        Self {
            shared,
            engine,
            cfg,
            cfg_path,
            config_warning,
            save_error: None,
            inputs,
            outputs,
            dirty_since: None,
        }
    }

    fn mark_dirty(&mut self) {
        apply_to_shared(&self.cfg, &self.shared);
        self.dirty_since = Some(Instant::now());
    }

    fn restart_audio(&mut self) {
        let _ = self.engine.send(Cmd::Restart(self.cfg.audio_settings()));
    }

    fn save_now(&mut self) {
        self.dirty_since = None;
        self.save_error = config::save(&self.cfg_path, &self.cfg)
            .err()
            .map(|e| format!("Не удалось сохранить настройки: {e:#}"));
    }

    fn reload_config(&mut self) {
        let (cfg, warn) = config::load(&self.cfg_path);
        let audio_changed = cfg.audio_settings() != self.cfg.audio_settings();
        self.cfg = cfg;
        self.config_warning = warn;
        apply_to_shared(&self.cfg, &self.shared);
        if audio_changed {
            self.restart_audio();
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
            if self.shared.capturing.load(Ordering::Relaxed) {
                ui.label(RichText::new("нажми любую клавишу… (Esc — отмена)").color(ORANGE));
            } else {
                let name = if self.cfg.hotkey.key.trim().is_empty() {
                    "нет".to_string()
                } else {
                    self.cfg.hotkey.key.clone()
                };
                ui.label(RichText::new(name).strong().monospace());
                if ui.button("Назначить…").clicked() {
                    self.shared.captured_vk.store(0, Ordering::Relaxed);
                    self.shared.capturing.store(true, Ordering::Relaxed);
                }
                if !self.cfg.hotkey.key.is_empty() && ui.button("Убрать").clicked() {
                    self.cfg.hotkey.key.clear();
                    self.mark_dirty();
                }
            }
        });

        // Клавиша поймана потоком горячих клавиш.
        let captured = self.shared.captured_vk.swap(0, Ordering::Relaxed);
        if captured != 0 && captured != CAPTURE_CANCELLED {
            self.cfg.hotkey.key = keys::key_name(captured);
            self.mark_dirty();
        }
        if self.shared.capturing.load(Ordering::Relaxed) {
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }

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
            ui.add_enabled_ui(self.cfg.audio.monitor, |ui| {
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

            ui.label("Громкость прослушки:");
            let mut vol = self.cfg.audio.monitor_volume;
            ui.add_enabled(self.cfg.audio.monitor, egui::Slider::new(&mut vol, 0..=100).suffix("%"));
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
                ui.weak(format!("Прослушка: {m}"));
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
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.lag_block(ui);
                ui.separator();
                ui.label(RichText::new("Устройства").strong());
                self.devices_block(ui);
                ui.separator();
                self.meters_block(ui);
                ui.separator();
                self.status_block(ui);
                ui.separator();
                egui::CollapsingHeader::new("Как подключить к Discord").show(ui, |ui| {
                    ui.label("1. Установи VB-Audio Virtual Cable (vb-audio.com) и перезагрузи компьютер.");
                    ui.label("2. Здесь выбери выход «CABLE Input (VB-Audio Virtual Cable)».");
                    ui.label("3. В Discord: Настройки → Голос и видео → Устройство ввода: «CABLE Output».");
                    ui.label("4. Там же выключи шумоподавление Krisp (или поставь «Стандартное») —");
                    ui.label("    иначе Discord будет «чистить» звук лага.");
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

        // Сохраняем через полсекунды после последнего изменения (чтобы не писать
        // файл на каждое движение ползунка).
        if let Some(t) = self.dirty_since {
            if t.elapsed() > Duration::from_millis(500) {
                self.save_now();
            } else {
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
        }
        // Индикаторы уровня обновляются ~30 раз в секунду.
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }

    fn on_exit(&mut self) {
        if self.dirty_since.is_some() {
            self.save_now();
        }
        let _ = self.engine.send(Cmd::Shutdown);
    }
}
