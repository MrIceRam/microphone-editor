//! Окно программы (egui).
//!
//! Сверху всегда видна «цепочка сигнала»: Микрофон → Лаг → Discord, с уровнями
//! звука и большой кнопкой лага посередине. Под ней — вкладки с настройками.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, Frame, Layout, Margin, Painter, Pos2, Rect, RichText,
    Sense, Shape, Stroke, StrokeKind, Vec2, pos2, vec2,
};

use crate::config::{self, Catchup, Config, HotkeyMode, Tab};
use crate::engine::{self, AudioSettings, Cmd};
use crate::keys;
use crate::lag::LagMode;
use crate::midi;
use crate::notes;
use crate::overlay::OverlayState;
use crate::piano;
use crate::state::LockExt;
use crate::state::{CAPTURE_CANCELLED, Shared};
use crate::theme::{self, LAG, LINE, MUTED, PANEL, RAISED, RAISED_HOVER, SIGNAL, SIGNAL_DIM, SUNKEN, TEXT, WARN};
use crate::tray::{Tray, TrayAction};
use crate::tts;
use crate::twitch::{self, ChatState};
use crate::virtual_mic::{self, LoopbackResult, VirtualMic};
use crate::voice_fx::VoiceFx;

const CABLE_SITE: &str = "https://vb-audio.com/Cable/";

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
    /// Ждём ноту-переключатель.
    WaitToggle,
}

/// Как нарисовать связь между узлами цепочки.
#[derive(Clone, Copy)]
enum Link {
    /// Звук идёт.
    Clean(Color32),
    /// Включён лаг — рваная линия.
    Jagged(Color32),
    /// Связи нет.
    Dashed(Color32),
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
    /// Раскрыть инструкцию по VB-Cable на вкладке «Устройства».
    open_cable_help: bool,
    /// Проверка виртуального микрофона (идёт в отдельном потоке).
    check: Arc<Mutex<CheckState>>,
    /// Значок в трее (None — не удалось создать, тогда окно ведёт себя обычно).
    tray: Option<Tray>,
    /// Окно спрятано в трей.
    hidden: bool,
    /// Выход через меню трея (а не сворачивание).
    quitting: bool,
    /// Голоса Windows для озвучки.
    voices: Vec<String>,
    /// В прошлый раз программа упала — что было.
    crash_notice: Option<String>,
    /// Значок программы для боковой панели.
    logo: Option<egui::TextureHandle>,
}

/// Состояние проверки виртуального микрофона.
enum CheckState {
    Idle,
    Running,
    Done {
        render: String,
        result: Result<LoopbackResult, String>,
        /// Результат уже записан в настройки.
        applied: bool,
    },
}

/// Как показать микрофон кабеля: «Micro Edition», если переименован,
/// иначе название драйвера («Steam Streaming Microphone»).
fn mic_display_name(capture: &str) -> String {
    let name = virtual_mic::name_part(capture);
    let generic = ["микрофон", "microphone", "cable output", "линия", "line"];
    if generic
        .iter()
        .any(|g| name.eq_ignore_ascii_case(g) || name.to_lowercase() == *g)
    {
        virtual_mic::driver_part(capture).unwrap_or(name).to_string()
    } else {
        name.to_string()
    }
}

/// Переносит настройки из конфига в общее состояние (кроме устройств).
pub fn apply_to_shared(cfg: &Config, shared: &Shared) {
    shared.lag_mode.store(cfg.lag.mode as u8, Ordering::Relaxed);
    shared.intensity.set(cfg.lag.intensity as f32 / 100.0);
    shared
        .catchup_skip
        .store(cfg.lag.catchup == Catchup::Skip, Ordering::Relaxed);
    shared.catchup_speed.set(cfg.lag.catchup_speed);
    shared.bitrate.store(cfg.lag.bitrate, Ordering::Relaxed);
    shared.input_gain.set(10f32.powf(cfg.audio.input_gain_db / 20.0));
    shared.monitor_volume.set(cfg.audio.monitor_volume as f32 / 100.0);
    shared.main_gain.set(10f32.powf(-cfg.audio.cable_gain_db / 20.0));
    shared
        .hotkey_vk
        .store(keys::parse_key(&cfg.hotkey.key).unwrap_or(0), Ordering::Relaxed);
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
    shared
        .monitor_piano
        .store(cfg.piano.enabled && cfg.piano.monitor, Ordering::Relaxed);

    let e = &cfg.effects;
    shared.voice_fx.store(e.fx as u8, Ordering::Relaxed);
    shared.fx_mix.set(e.mix as f32 / 100.0);
    shared.denoise.store(e.denoise, Ordering::Relaxed);
    shared.denoise_amount.set(e.denoise_amount as f32 / 100.0);

    let o = &cfg.overlay;
    shared.overlay_enabled.store(o.enabled, Ordering::Relaxed);
    shared.overlay_port.store(o.port, Ordering::Relaxed);
    shared
        .overlay_from
        .store(notes::parse_note(&o.from).unwrap_or(36), Ordering::Relaxed);
    shared
        .overlay_to
        .store(notes::parse_note(&o.to).unwrap_or(96), Ordering::Relaxed);
    shared.overlay_names.store(o.names, Ordering::Relaxed);

    let t = &cfg.twitch;
    shared.twitch_enabled.store(t.enabled, Ordering::Relaxed);
    *shared.twitch_channel.lock_ok() = t.channel.clone();
    *shared.twitch_reward.lock_ok() = t.reward_id.clone();
    *shared.tts_voice.lock_ok() = t.voice.clone();
    shared.tts_volume.set(t.volume as f32 / 100.0);
    shared.tts_read_name.store(t.read_name, Ordering::Relaxed);
    shared.tts_max_len.store(t.max_length, Ordering::Relaxed);
    shared.tts_to_mic.store(t.to_mic, Ordering::Relaxed);
    shared.tts_to_headphones.store(t.to_headphones, Ordering::Relaxed);

    *shared.midi_device.lock_ok() = cfg.midi.device.clone();
    let (map, _) = cfg.keymap_table();
    let mut km = shared.keymapper.lock_ok();
    km.map = map;
    km.pedal_vk = keys::parse_key(&cfg.midi.pedal_key).unwrap_or(0);
    km.set_enabled(cfg.midi.send_keys);
    shared.keys_enabled.store(cfg.midi.send_keys, Ordering::Relaxed);
    shared.toggle_note.store(
        notes::parse_note(&cfg.midi.toggle_note).map(i16::from).unwrap_or(-1),
        Ordering::Relaxed,
    );
}

// ---------------------------------------------------------------------------
// Мелкие помощники для рисования
// ---------------------------------------------------------------------------

/// "Микрофон (DP-HS-1015)" -> "DP-HS-1015".
fn friendly_device(name: &str) -> &str {
    match (name.find('('), name.ends_with(')')) {
        (Some(a), true) if a + 1 < name.len() - 1 => &name[a + 1..name.len() - 1],
        _ => name,
    }
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        chars[..max.saturating_sub(1)].iter().collect::<String>() + "…"
    }
}

/// Уровень (0..1) -> длина полоски (шкала -60..0 дБ).
fn level_to_bar(level: f32) -> f32 {
    let db = 20.0 * level.max(1e-6).log10();
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Как показывать клавишу на экранном пианино.
fn short_key_name(name: &str) -> String {
    match name {
        "Mouse1" => "ЛКМ".into(),
        "Mouse2" => "ПКМ".into(),
        "Mouse3" => "СКМ".into(),
        "Mouse4" => "М4".into(),
        "Mouse5" => "М5".into(),
        "LShift" | "RShift" | "Shift" => "Shift".into(),
        "LCtrl" | "RCtrl" | "Ctrl" => "Ctrl".into(),
        "LAlt" | "RAlt" | "Alt" => "Alt".into(),
        "Space" => "Пробел".into(),
        other => truncate(other, 5),
    }
}

fn section(ui: &mut egui::Ui, text: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(text).font(theme::semibold(16.0)).color(TEXT));
    ui.add_space(2.0);
}

fn muted(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).color(MUTED).size(13.0));
}

/// Цветная плашка с сообщением. Возвращает true, если нажали кнопку действия.
fn banner(ui: &mut egui::Ui, color: Color32, text: &str, action: Option<&str>) -> bool {
    let mut clicked = false;
    Frame::new()
        .fill(color.gamma_multiply(0.13))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.55)))
        .corner_radius(8)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let button_w = if action.is_some() { 160.0 } else { 0.0 };
            ui.horizontal(|ui| {
                let text_w = ui.available_width() - button_w;
                ui.allocate_ui(vec2(text_w, 0.0), |ui| {
                    ui.add(egui::Label::new(RichText::new(text).color(TEXT)).wrap());
                });
                if let Some(a) = action {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        clicked = ui.button(a).clicked();
                    });
                }
            });
        });
    ui.add_space(4.0);
    clicked
}

/// Переключатель из нескольких вариантов (кнопки встык).
fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (v, label) in options {
            let selected = *value == *v;
            let button = egui::Button::new(RichText::new(*label).color(if selected { TEXT } else { MUTED }))
                .fill(if selected { SIGNAL_DIM } else { PANEL })
                .stroke(Stroke::new(1.0, if selected { SIGNAL } else { LINE }));
            if ui.add(button).clicked() && !selected {
                *value = *v;
                changed = true;
            }
        }
    });
    changed
}

/// Плитка режима лага.
fn mode_tile(ui: &mut egui::Ui, mode: LagMode, selected: bool, width: f32) -> bool {
    tile(ui, mode.label(), mode.description(), selected, width)
}

/// Сетка плиток в `cols` столбцов. Возвращает индекс нажатой.
fn tile_grid(ui: &mut egui::Ui, items: &[(&str, &str, bool)], cols: usize) -> Option<usize> {
    let gap = 8.0;
    let w = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
    let mut clicked = None;
    for (r, row) in items.chunks(cols).enumerate() {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (c, (title, desc, selected)) in row.iter().enumerate() {
                if tile(ui, title, desc, *selected, w) {
                    clicked = Some(r * cols + c);
                }
            }
        });
    }
    clicked
}

/// Плитка с заголовком и подписью.
fn tile(ui: &mut egui::Ui, title: &str, description: &str, selected: bool, width: f32) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 58.0), Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let p = ui.painter();
    let fill = if selected {
        SIGNAL_DIM
    } else if resp.hovered() {
        RAISED
    } else {
        PANEL
    };
    p.rect_filled(rect, 8.0, fill);
    p.rect_stroke(
        rect,
        8.0,
        Stroke::new(if selected { 1.5 } else { 1.0 }, if selected { SIGNAL } else { LINE }),
        StrokeKind::Inside,
    );
    p.text(
        rect.left_top() + vec2(12.0, 9.0),
        Align2::LEFT_TOP,
        title,
        theme::semibold(15.0),
        TEXT,
    );
    p.text(
        rect.left_top() + vec2(12.0, 32.0),
        Align2::LEFT_TOP,
        description,
        theme::regular(12.5),
        if selected { TEXT.gamma_multiply(0.8) } else { MUTED },
    );
    resp.clicked()
}

/// Пункт бокового меню.
fn sidebar_item(ui: &mut egui::Ui, label: &str, selected: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 8.0, RAISED);
        p.rect_filled(
            Rect::from_min_size(rect.left_top() + vec2(0.0, 8.0), vec2(3.0, rect.height() - 16.0)),
            2.0,
            SIGNAL,
        );
    } else if resp.hovered() {
        p.rect_filled(rect, 8.0, RAISED.gamma_multiply(0.6));
    }
    p.text(
        rect.left_center() + vec2(14.0, 0.0),
        Align2::LEFT_CENTER,
        label,
        if selected {
            theme::semibold(15.0)
        } else {
            theme::regular(15.0)
        },
        if selected { TEXT } else { MUTED },
    );
    resp.clicked()
}

/// Строка состояния в боковой панели: цветная точка и текст.
fn status_line(ui: &mut egui::Ui, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        ui.painter().circle_filled(dot.center(), 3.5, color);
        ui.label(
            RichText::new(text)
                .size(12.5)
                .color(if color == MUTED { MUTED } else { TEXT }),
        );
    });
}

/// Размеры MIDI-клавиатур для оверлея: (название, первая нота, последняя).
const KEYBOARD_SIZES: &[(&str, u8, u8)] = &[
    ("25 клавиш (C3–C5)", 48, 72),
    ("37 клавиш (C3–C6)", 48, 84),
    ("49 клавиш (C2–C6)", 36, 84),
    ("61 клавиша (C2–C7)", 36, 96),
    ("76 клавиш (E1–G7)", 28, 103),
    ("88 клавиш (A0–C8)", 21, 108),
];

fn draw_link(p: &Painter, from: Pos2, to: Pos2, link: Link, time: f64) {
    match link {
        Link::Clean(c) => {
            p.line_segment([from, to], Stroke::new(2.0, c));
        }
        Link::Dashed(c) => {
            let mut x = from.x;
            while x < to.x {
                let x2 = (x + 6.0).min(to.x);
                p.line_segment([pos2(x, from.y), pos2(x2, from.y)], Stroke::new(2.0, c));
                x += 11.0;
            }
        }
        Link::Jagged(c) => {
            let mut points = vec![from];
            let mut x = from.x + 6.0;
            let mut i = 0;
            while x < to.x - 6.0 {
                let jitter = 0.55 + 0.45 * ((time * 23.0 + i as f64 * 1.7).sin() as f32);
                let dy = if i % 2 == 0 { -7.0 } else { 7.0 } * jitter;
                points.push(pos2(x, from.y + dy));
                x += 7.0;
                i += 1;
            }
            points.push(to);
            p.add(Shape::line(points, Stroke::new(2.0, c)));
        }
    }
    // Стрелка: куда идёт звук.
    let color = match link {
        Link::Clean(c) | Link::Jagged(c) | Link::Dashed(c) => c,
    };
    p.add(Shape::convex_polygon(
        vec![
            pos2(to.x - 1.0, to.y),
            pos2(to.x - 8.0, to.y - 5.0),
            pos2(to.x - 8.0, to.y + 5.0),
        ],
        color,
        Stroke::NONE,
    ));
}

fn draw_node(p: &Painter, r: Rect, title: &str, sub: &str, level: f32, color: Color32) {
    p.rect_filled(r, 10.0, PANEL);
    p.rect_stroke(r, 10.0, Stroke::new(1.0, LINE), StrokeKind::Inside);
    let pad = 12.0;
    p.text(
        r.left_top() + vec2(pad, 9.0),
        Align2::LEFT_TOP,
        title,
        theme::semibold(15.0),
        TEXT,
    );
    p.text(
        r.left_top() + vec2(pad, 31.0),
        Align2::LEFT_TOP,
        sub,
        theme::regular(12.5),
        MUTED,
    );
    let bar = Rect::from_min_size(
        pos2(r.left() + pad, r.bottom() - 16.0),
        vec2(r.width() - 2.0 * pad, 6.0),
    );
    p.rect_filled(bar, 3.0, SUNKEN);
    let f = level_to_bar(level);
    if f > 0.02 {
        p.rect_filled(
            Rect::from_min_size(bar.min, vec2(bar.width() * f, bar.height())),
            3.0,
            color,
        );
    }
}

// ---------------------------------------------------------------------------
// Приложение
// ---------------------------------------------------------------------------

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shared: Arc<Shared>,
        engine: Sender<Cmd>,
        cfg: Config,
        cfg_path: PathBuf,
        config_warning: Option<String>,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        let _ = shared.egui_ctx.set(cc.egui_ctx.clone());
        let (inputs, outputs) = engine::list_devices();
        let last_audio = cfg.audio_settings();
        let mut config_warning = config_warning;
        let tray = match Tray::new(&cc.egui_ctx) {
            Ok(t) => Some(t),
            Err(e) => {
                config_warning = Some(format!("Не удалось создать значок в трее: {e:#}"));
                None
            }
        };
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
            open_cable_help: false,
            check: Arc::new(Mutex::new(CheckState::Idle)),
            tray,
            hidden: false,
            quitting: false,
            voices: tts::voice_names(),
            crash_notice: crate::crash::take_pending(),
            logo: Some(cc.egui_ctx.load_texture(
                "logo",
                egui::ColorImage::from_rgba_unmultiplied([64, 64], &crate::tray::icon_rgba(64, false)),
                egui::TextureOptions::LINEAR,
            )),
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

    // --- Шапка: цепочка сигнала -----------------------------------------------

    fn chain(&mut self, ui: &mut egui::Ui) {
        let st = self.shared.status();
        let width = ui.available_width();
        let height = 100.0;
        let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
        let painter = ui.painter_at(rect.expand(4.0));

        let side_w = (width * 0.27).clamp(140.0, 200.0);
        let center_w = (width * 0.27).clamp(150.0, 190.0);
        let node_h = 72.0;
        let cy = rect.center().y;
        let left = Rect::from_min_size(pos2(rect.left(), cy - node_h / 2.0), vec2(side_w, node_h));
        let right = Rect::from_min_size(pos2(rect.right() - side_w, cy - node_h / 2.0), vec2(side_w, node_h));
        let center = Rect::from_center_size(pos2(rect.center().x, cy), vec2(center_w, height - 4.0));

        let active = self.shared.lag_active();
        let wet = self.shared.lag_wet.get();
        let time = ui.input(|i| i.time);

        // Связи.
        let mic_ok = st.running && st.error.is_none();
        let left_link = if mic_ok {
            Link::Clean(SIGNAL)
        } else {
            Link::Dashed(MUTED)
        };
        let right_link = if !mic_ok {
            Link::Dashed(MUTED)
        } else if st.output_name.is_none() {
            Link::Dashed(WARN)
        } else if active || wet > 0.01 {
            Link::Jagged(LAG)
        } else {
            Link::Clean(SIGNAL)
        };
        draw_link(
            &painter,
            pos2(left.right() + 6.0, cy),
            pos2(center.left() - 6.0, cy),
            left_link,
            time,
        );
        draw_link(
            &painter,
            pos2(center.right() + 6.0, cy),
            pos2(right.left() - 6.0, cy),
            right_link,
            time,
        );

        // Узел «Микрофон».
        let max_chars = ((side_w - 24.0) / 7.0) as usize;
        let mic_sub = if let Some(e) = &st.error {
            truncate(e, max_chars)
        } else if st.input_name.is_empty() {
            "запуск…".to_string()
        } else {
            truncate(friendly_device(&st.input_name), max_chars)
        };
        draw_node(
            &painter,
            left,
            "Микрофон",
            &mic_sub,
            self.shared.in_level.get(),
            if mic_ok { SIGNAL } else { LAG },
        );

        // Узел «Discord и игры» (виртуальный микрофон) / «Выход» (обычное устройство).
        let (title, sub, color) = match (&st.output_name, &st.virtual_capture) {
            (Some(_), Some(cap)) => ("Discord и игры", truncate(&mic_display_name(cap), max_chars), SIGNAL),
            (Some(n), None) => ("Выход", truncate(friendly_device(n), max_chars), SIGNAL),
            (None, _) => ("Discord и игры", "нет виртуального микрофона".to_string(), WARN),
        };
        let color = if active { LAG } else { color };
        draw_node(&painter, right, title, &sub, self.shared.out_level.get(), color);

        // Кнопка лага.
        let resp = ui
            .interact(center, egui::Id::NULL.with("lag-button"), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text("Включить или выключить лаг");
        if resp.clicked() {
            self.shared.lag_latched.fetch_xor(true, Ordering::Relaxed);
        }
        let fill = if active {
            LAG
        } else if resp.hovered() {
            RAISED_HOVER
        } else {
            RAISED
        };
        painter.rect_filled(center, 14.0, fill);
        if !active {
            painter.rect_stroke(center, 14.0, Stroke::new(1.0, LINE), StrokeKind::Inside);
        }
        let key = self.cfg.hotkey.key.trim();
        let hint = if key.is_empty() {
            "клик — включить".to_string()
        } else if self.cfg.hotkey.mode == HotkeyMode::Hold {
            format!("удерживай {key}")
        } else {
            format!("нажимай {key}")
        };
        painter.text(
            center.center() - vec2(0.0, 10.0),
            Align2::CENTER_CENTER,
            if active {
                "Лаг включён"
            } else {
                "Лаг выключен"
            },
            theme::semibold(20.0),
            if active { Color32::WHITE } else { TEXT },
        );
        painter.text(
            center.center() + vec2(0.0, 16.0),
            Align2::CENTER_CENTER,
            hint,
            theme::regular(13.0),
            if active {
                Color32::WHITE.gamma_multiply(0.85)
            } else {
                MUTED
            },
        );
    }

    fn banners(&mut self, ui: &mut egui::Ui) {
        if let Some(c) = self.crash_notice.clone() {
            let text = format!(
                "В прошлый раз программа упала: {}. Подробности записаны в crash.log.",
                truncate(&c, 140)
            );
            if banner(ui, LAG, &text, Some("Открыть журнал")) {
                let _ = std::process::Command::new("notepad")
                    .arg(crate::crash::log_path())
                    .spawn();
                self.crash_notice = None;
            }
        }
        if self.shared.piano_failed.load(Ordering::Relaxed) {
            banner(
                ui,
                WARN,
                "Синтезатор пианино отключился из-за ошибки (она записана в crash.log). Голос и лаг работают. \
                 Чтобы вернуть звук пианино, выключи и включи «Играть звук пианино».",
                None,
            );
        }
        let st = self.shared.status();
        if let Some(err) = &st.error {
            banner(
                ui,
                LAG,
                &format!("Звук не запустился: {err}. Пробую снова каждые 2 секунды."),
                None,
            );
        } else if st.running && st.output_name.is_none() {
            let (text, action) = if self.cfg.audio.output_device.trim().is_empty() {
                (
                    "Discord и игры пока ничего не слышат: в системе нет виртуального микрофона.",
                    "Как установить",
                )
            } else {
                (
                    "Выход, выбранный в настройках, не найден: Discord и игры ничего не слышат.",
                    "Выбрать",
                )
            };
            if banner(ui, WARN, text, Some(action)) {
                self.cfg.ui.tab = Tab::Devices;
                self.open_cable_help = true;
                self.mark_dirty();
            }
        }
        // Звук идёт в виртуальный микрофон, но громкость кабеля ещё не выровнена.
        if let CheckState::Running = *self.check.lock_ok() {
            banner(
                ui,
                SIGNAL,
                "Проверяю виртуальный микрофон и выравниваю громкость…",
                None,
            );
        }
        if let Some(w) = self.config_warning.clone() {
            banner(ui, WARN, &w, None);
        }
        if let Some(e) = self.save_error.clone() {
            banner(ui, LAG, &e, None);
        }
    }

    /// Боковая панель: вкладки сверху, состояние снизу.
    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(logo) = &self.logo {
                ui.add(egui::Image::new(logo).fit_to_exact_size(vec2(26.0, 26.0)));
            }
            ui.label(
                RichText::new("Microphone Editor")
                    .font(theme::semibold(15.5))
                    .color(TEXT),
            );
        });
        ui.add_space(16.0);
        ui.spacing_mut().item_spacing.y = 4.0;
        for t in Tab::ALL {
            let selected = self.cfg.ui.tab == t;
            if sidebar_item(ui, t.label(), selected) && !selected {
                self.cfg.ui.tab = t;
                self.mark_dirty();
            }
        }

        // Состояние — внизу панели (снизу вверх).
        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            let st = self.shared.status();
            if let OverlayState::Running(port) = self.shared.overlay_state.lock_ok().clone() {
                status_line(ui, MUTED, &format!("Оверлей: localhost:{port}"));
            }
            if let Some(m) = self.shared.midi_status.lock_ok().connected.clone() {
                status_line(ui, SIGNAL, &format!("Пианино: {}", truncate(&m, 18)));
            }
            if let Some(now) = self.shared.tts_now.lock_ok().clone() {
                status_line(ui, SIGNAL, &format!("Озвучиваю: {}", truncate(&now, 16)));
            }
            if self.shared.keys_enabled.load(Ordering::Relaxed) {
                status_line(ui, SIGNAL, "Клавиши для игр включены");
            }
            let glitches: u64 = [
                &self.shared.input_stats,
                &self.shared.main_stats,
                &self.shared.monitor_stats,
            ]
            .iter()
            .map(|s| s.underruns_total.load(Ordering::Relaxed))
            .sum();
            if glitches > 0 {
                status_line(ui, WARN, &format!("Сбоев звука: {glitches}"));
            } else {
                status_line(ui, MUTED, "Сбоев звука нет");
            }
            let (color, text) = if st.error.is_some() {
                (LAG, "Звук не запущен")
            } else if st.running {
                (SIGNAL, "Звук работает")
            } else {
                (MUTED, "Запуск звука…")
            };
            status_line(ui, color, text);
        });
    }

    /// Две колонки с промежутком: слева — основное, справа — дополнительное.
    fn two_columns(
        &mut self,
        ui: &mut egui::Ui,
        left: impl FnOnce(&mut Self, &mut egui::Ui),
        right: impl FnOnce(&mut Self, &mut egui::Ui),
    ) {
        ui.spacing_mut().item_spacing.x = 36.0;
        ui.columns(2, |cols| {
            // Обычная раскладка сверху вниз: без неё egui растягивает текст по ширине.
            let plain = Layout::top_down(Align::Min);
            cols[0].spacing_mut().item_spacing.x = 8.0;
            cols[1].spacing_mut().item_spacing.x = 8.0;
            cols[0].with_layout(plain, |ui| left(self, ui));
            cols[1].with_layout(plain, |ui| right(self, ui));
        });
        ui.spacing_mut().item_spacing.x = 8.0;
    }

    // --- Вкладка «Лаг» ------------------------------------------------------------

    fn lag_tab(&mut self, ui: &mut egui::Ui) {
        self.two_columns(ui, Self::lag_modes, Self::lag_hotkey);
    }

    fn lag_modes(&mut self, ui: &mut egui::Ui) {
        section(ui, "Как лагает");
        let cols = if ui.available_width() > 560.0 { 3 } else { 2 };
        let gap = 8.0;
        let tile_w = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
        let mut chosen = None;
        for row in LagMode::ALL.chunks(cols) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for &m in row {
                    if mode_tile(ui, m, self.cfg.lag.mode == m, tile_w) {
                        chosen = Some(m);
                    }
                }
            });
        }
        if let Some(m) = chosen {
            if m != self.cfg.lag.mode {
                self.cfg.lag.mode = m;
                self.mark_dirty();
            }
        }

        ui.add_space(6.0);
        egui::Grid::new("lag-grid")
            .num_columns(2)
            .min_col_width(150.0)
            .spacing([12.0, 10.0])
            .show(ui, |ui| {
                ui.label("Сила");
                let mut intensity = self.cfg.lag.intensity;
                ui.add(egui::Slider::new(&mut intensity, 0..=100).suffix("%"));
                if intensity != self.cfg.lag.intensity {
                    self.cfg.lag.intensity = intensity;
                    self.mark_dirty();
                }
                ui.end_row();

                if matches!(self.cfg.lag.mode, LagMode::Spikes | LagMode::Mix) {
                    ui.label("После скачка пинга");
                    let mut c = self.cfg.lag.catchup;
                    if segmented(
                        ui,
                        &mut c,
                        &[
                            (Catchup::Speedup, "Догонять ускоренно"),
                            (Catchup::Skip, "Перескакивать"),
                        ],
                    ) {
                        self.cfg.lag.catchup = c;
                        self.mark_dirty();
                    }
                    ui.end_row();

                    if self.cfg.lag.catchup == Catchup::Speedup {
                        ui.label("Скорость догона");
                        let mut speed = self.cfg.lag.catchup_speed;
                        ui.add(egui::Slider::new(&mut speed, 1.1..=3.0).suffix("×").fixed_decimals(1));
                        if (speed - self.cfg.lag.catchup_speed).abs() > f32::EPSILON {
                            self.cfg.lag.catchup_speed = speed;
                            self.mark_dirty();
                        }
                        ui.end_row();
                    }
                }
            });
    }

    fn lag_hotkey(&mut self, ui: &mut egui::Ui) {
        section(ui, "Горячая клавиша");
        egui::Grid::new("hotkey-grid")
            .num_columns(2)
            .min_col_width(150.0)
            .spacing([12.0, 10.0])
            .show(ui, |ui| {
                ui.label("Клавиша");
                ui.horizontal(|ui| {
                    if self.capturing(CapturePurpose::Hotkey) {
                        ui.label(RichText::new("Нажми любую клавишу. Esc — отмена").color(WARN));
                    } else {
                        let name = if self.cfg.hotkey.key.trim().is_empty() {
                            "нет".to_string()
                        } else {
                            self.cfg.hotkey.key.clone()
                        };
                        key_chip(ui, &name);
                        if ui.button("Назначить").clicked() {
                            self.start_capture(CapturePurpose::Hotkey);
                        }
                        if !self.cfg.hotkey.key.is_empty() && ui.button("Убрать").clicked() {
                            self.cfg.hotkey.key.clear();
                            self.mark_dirty();
                        }
                    }
                });
                ui.end_row();

                ui.label("Как работает");
                let mut mode = self.cfg.hotkey.mode;
                if segmented(
                    ui,
                    &mut mode,
                    &[(HotkeyMode::Hold, "Пока держу"), (HotkeyMode::Toggle, "Вкл / выкл")],
                ) {
                    self.cfg.hotkey.mode = mode;
                    self.shared.lag_latched.store(false, Ordering::Relaxed);
                    self.mark_dirty();
                }
                ui.end_row();
            });
        muted(ui, "Клавиша работает в любой игре, и игра тоже видит нажатие.");

        section(ui, "Как это звучит");
        muted(
            ui,
            "Голос проходит через настоящий кодек Opus, как в Discord: часть пакетов «теряется» \
             и опаздывает. Поэтому лаг звучит так же, как у человека с плохим интернетом. \
             Пианино и озвучка Twitch по умолчанию не лагают.",
        );
    }

    // --- Вкладка «Эффекты голоса» ----------------------------------------------------

    fn effects_tab(&mut self, ui: &mut egui::Ui) {
        self.two_columns(ui, Self::effects_left, Self::effects_right);
    }

    fn effects_left(&mut self, ui: &mut egui::Ui) {
        section(ui, "Эффект голоса");
        let items: Vec<(&str, &str, bool)> = VoiceFx::ALL
            .iter()
            .map(|f| (f.label(), f.description(), self.cfg.effects.fx == *f))
            .collect();
        if let Some(i) = tile_grid(ui, &items, 2) {
            let fx = VoiceFx::ALL[i];
            if fx != self.cfg.effects.fx {
                self.cfg.effects.fx = fx;
                self.mark_dirty();
            }
        }
        ui.add_space(6.0);
        ui.add_enabled_ui(self.cfg.effects.fx != VoiceFx::None, |ui| {
            egui::Grid::new("fx-grid")
                .num_columns(2)
                .min_col_width(150.0)
                .spacing([12.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Сила эффекта");
                    let mut mix = self.cfg.effects.mix;
                    ui.add(egui::Slider::new(&mut mix, 0..=100).suffix("%"));
                    if mix != self.cfg.effects.mix {
                        self.cfg.effects.mix = mix;
                        self.mark_dirty();
                    }
                    ui.end_row();
                });
        });
        muted(
            ui,
            "Эффект работает всё время, пока выбран. Лаг включается поверх него.",
        );
    }

    fn effects_right(&mut self, ui: &mut egui::Ui) {
        section(ui, "Шумоподавление");
        let mut on = self.cfg.effects.denoise;
        ui.checkbox(&mut on, "Убирать шум с микрофона");
        if on != self.cfg.effects.denoise {
            self.cfg.effects.denoise = on;
            self.mark_dirty();
        }
        ui.add_enabled_ui(self.cfg.effects.denoise, |ui| {
            egui::Grid::new("dn-grid")
                .num_columns(2)
                .min_col_width(150.0)
                .spacing([12.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Сила");
                    let mut amount = self.cfg.effects.denoise_amount;
                    ui.add(egui::Slider::new(&mut amount, 0..=100).suffix("%"));
                    if amount != self.cfg.effects.denoise_amount {
                        self.cfg.effects.denoise_amount = amount;
                        self.mark_dirty();
                    }
                    ui.end_row();
                });
        });
        muted(
            ui,
            "Нейросеть RNNoise прямо в программе, без интернета: убирает гул, клавиатуру, вентилятор. \
             С ней шумоподавление Krisp в Discord можно выключить — оно портит лаг и пианино.",
        );

        section(ui, "Послушать себя");
        let mut monitor = self.cfg.audio.monitor;
        ui.checkbox(&mut monitor, "Слышать себя в наушниках");
        if monitor != self.cfg.audio.monitor {
            self.cfg.audio.monitor = monitor;
            self.mark_dirty();
        }
        muted(
            ui,
            "Так слышно, как звучишь в Discord. Во время стрима лучше выключить: голос попадёт в OBS дважды.",
        );
    }

    // --- Вкладка «Пианино» --------------------------------------------------------

    fn midi_status_line(&mut self, ui: &mut egui::Ui) {
        let midi = self.shared.midi_status.lock_ok().clone();
        ui.horizontal(|ui| {
            let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
            match &midi.connected {
                Some(n) => {
                    ui.painter().circle_filled(dot.center(), 4.0, SIGNAL);
                    ui.label(RichText::new(n).color(TEXT));
                }
                None => {
                    ui.painter().circle_filled(dot.center(), 4.0, MUTED);
                    ui.label(RichText::new("MIDI-клавиатура не подключена").color(TEXT));
                    muted(ui, "подключи по USB, программа найдёт её сама");
                }
            }
        });
        if midi.ports.len() > 1 || !self.cfg.midi.device.is_empty() {
            ui.horizontal(|ui| {
                ui.label("Какую использовать");
                egui::ComboBox::from_id_salt("midi")
                    .selected_text(if self.cfg.midi.device.is_empty() {
                        "Первую найденную".to_string()
                    } else {
                        self.cfg.midi.device.clone()
                    })
                    .show_ui(ui, |ui| {
                        let mut v = self.cfg.midi.device.clone();
                        ui.selectable_value(&mut v, String::new(), "Первую найденную");
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
            banner(ui, WARN, e, None);
        }
    }

    fn piano_tab(&mut self, ui: &mut egui::Ui) {
        section(ui, "MIDI-клавиатура");
        self.midi_status_line(ui);

        self.piano_header(ui, "Экранное пианино");
        self.draw_piano(ui, false);
        ui.horizontal(|ui| {
            let last = self.shared.last_note.load(Ordering::Relaxed);
            if last >= 0 {
                let n = last as u8;
                muted(
                    ui,
                    format!(
                        "Последняя нота {} ({}), сила {}",
                        notes::note_name(n),
                        notes::note_name_ru(n),
                        self.shared.last_velocity.load(Ordering::Relaxed)
                    ),
                );
            } else {
                muted(ui, "Кликай по клавишам мышкой, чтобы проверить звук");
            }
            if self.shared.pedal_down.load(Ordering::Relaxed) {
                ui.label(RichText::new("педаль").size(13.0).color(SIGNAL));
            }
        });

        self.two_columns(ui, Self::piano_sound, Self::piano_extra);
    }

    fn piano_sound(&mut self, ui: &mut egui::Ui) {
        section(ui, "Звук");
        let mut enabled = self.cfg.piano.enabled;
        ui.checkbox(&mut enabled, "Играть звук пианино в микрофон");
        if enabled != self.cfg.piano.enabled {
            self.cfg.piano.enabled = enabled;
            self.mark_dirty();
        }
        for w in self
            .shared
            .status()
            .warnings
            .iter()
            .filter(|w| w.starts_with("Пианино"))
        {
            banner(ui, WARN, w, None);
        }

        ui.add_enabled_ui(self.cfg.piano.enabled, |ui| {
            egui::Grid::new("piano-grid")
                .num_columns(2)
                .min_col_width(130.0)
                .spacing([12.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Инструмент");
                    egui::ComboBox::from_id_salt("instrument")
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

                    ui.label("Громкость");
                    let mut vol = self.cfg.piano.volume;
                    ui.add(egui::Slider::new(&mut vol, 0..=100).suffix("%"));
                    if vol != self.cfg.piano.volume {
                        self.cfg.piano.volume = vol;
                        self.mark_dirty();
                    }
                    ui.end_row();

                    ui.label("Сдвиг высоты");
                    let mut tr = self.cfg.piano.transpose;
                    ui.add(egui::Slider::new(&mut tr, -24..=24).suffix(" полутонов"));
                    if tr != self.cfg.piano.transpose {
                        self.cfg.piano.transpose = tr;
                        self.mark_dirty();
                    }
                    ui.end_row();
                });
        });
    }

    fn piano_extra(&mut self, ui: &mut egui::Ui) {
        section(ui, "Набор звуков");
        ui.add_enabled_ui(self.cfg.piano.enabled, |ui| {
            egui::Grid::new("sf-grid").num_columns(1).show(ui, |ui| {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("soundfont")
                        .width(220.0)
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
                    if ui
                        .button("Папка")
                        .on_hover_text("Открыть папку с файлами .sf2, туда можно положить свои")
                        .clicked()
                    {
                        let dir = piano::soundfont_folder();
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                    }
                    if ui.button("⟳").on_hover_text("Обновить список файлов").clicked() {
                        self.soundfonts = piano::list_soundfonts();
                    }
                });
                ui.end_row();
            });
            muted(
                ui,
                "Файлы SoundFont (.sf2) из папки soundfonts. Свои можно положить туда же.",
            );

            section(ui, "Куда звучит");
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
    }

    /// Заголовок над экранным пианино со стрелками выбора октав.
    fn piano_header(&mut self, ui: &mut egui::Ui, title: &str) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).font(theme::semibold(16.0)).color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("›").on_hover_text("Октавой выше").clicked() && self.screen_octave < 7 {
                    self.screen_octave += 1;
                }
                let base = ((self.screen_octave + 1) * 12) as u8;
                ui.label(
                    RichText::new(format!("{}–{}", notes::note_name(base), notes::note_name(base + 24)))
                        .color(MUTED)
                        .size(13.0),
                );
                if ui.button("‹").on_hover_text("Октавой ниже").clicked() && self.screen_octave > 0 {
                    self.screen_octave -= 1;
                }
            });
        });
    }

    /// Экранное пианино на 2 октавы. `show_keys` — подписывать назначенные клавиши.
    fn draw_piano(&mut self, ui: &mut egui::Ui, show_keys: bool) {
        let base = ((self.screen_octave + 1) * 12) as u8;
        let last = base + 24;
        let width = ui.available_width().min(680.0);
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 112.0), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let keymap = if show_keys {
            Some(self.cfg.keymap_table().0)
        } else {
            None
        };

        let whites: Vec<u8> = (base..=last).filter(|&n| !notes::is_black(n)).collect();
        let ww = rect.width() / whites.len() as f32;
        let white_rects: Vec<(u8, Rect)> = whites
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let r = Rect::from_min_size(pos2(rect.left() + i as f32 * ww, rect.top()), vec2(ww, rect.height()));
                (n, r)
            })
            .collect();
        let black_rects: Vec<(u8, Rect)> = (base..=last)
            .filter(|&n| notes::is_black(n))
            .filter_map(|n| {
                let i = whites.iter().position(|&w| w == n - 1)?;
                let x = rect.left() + (i + 1) as f32 * ww;
                Some((
                    n,
                    Rect::from_center_size(
                        pos2(x, rect.top() + rect.height() * 0.31),
                        vec2(ww * 0.6, rect.height() * 0.62),
                    ),
                ))
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

        let toggle = if show_keys {
            notes::parse_note(&self.cfg.midi.toggle_note)
        } else {
            None
        };
        let key_label = |n: u8| -> Option<String> {
            if Some(n) == toggle {
                return Some(
                    if notes::is_black(n) {
                        "вкл"
                    } else {
                        "вкл/выкл"
                    }
                    .to_string(),
                );
            }
            let vk = keymap.as_ref()?[n as usize];
            (vk != 0).then(|| short_key_name(&keys::key_name(vk)))
        };

        painter.rect_filled(rect, 6.0, SUNKEN);
        for (n, r) in &white_rects {
            let label = key_label(*n);
            let fill = if self.shared.note_is_down(*n) {
                SIGNAL
            } else if Some(*n) == toggle {
                Color32::from_rgb(0xF6, 0xDF, 0xB6)
            } else if label.is_some() {
                Color32::from_rgb(0xCF, 0xEE, 0xE9)
            } else {
                Color32::from_rgb(0xEC, 0xEF, 0xF4)
            };
            let key = r.shrink2(vec2(1.0, 1.0));
            painter.rect_filled(
                key,
                CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 5,
                    se: 5,
                },
                fill,
            );
            if let Some(l) = label {
                painter.text(
                    pos2(r.center().x, r.bottom() - 26.0),
                    Align2::CENTER_BOTTOM,
                    l,
                    theme::semibold(12.5),
                    Color32::from_rgb(0x16, 0x4E, 0x48),
                );
            }
            if n % 12 == 0 {
                painter.text(
                    pos2(r.center().x, r.bottom() - 5.0),
                    Align2::CENTER_BOTTOM,
                    notes::note_name(*n),
                    theme::regular(11.0),
                    Color32::from_rgb(0x70, 0x78, 0x88),
                );
            }
        }
        for (n, r) in &black_rects {
            let label = key_label(*n);
            let fill = if self.shared.note_is_down(*n) {
                SIGNAL
            } else if Some(*n) == toggle {
                Color32::from_rgb(0x7A, 0x55, 0x1C)
            } else if label.is_some() {
                SIGNAL_DIM
            } else {
                Color32::from_rgb(0x12, 0x16, 0x1F)
            };
            painter.rect_filled(
                *r,
                CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 4,
                    se: 4,
                },
                fill,
            );
            if let Some(l) = label {
                painter.text(
                    pos2(r.center().x, r.bottom() - 5.0),
                    Align2::CENTER_BOTTOM,
                    l,
                    theme::semibold(11.0),
                    Color32::WHITE,
                );
            }
        }
    }

    // --- Вкладка «Клавиши для игр» -------------------------------------------------

    fn keys_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        let mut send = self.cfg.midi.send_keys;
        ui.checkbox(
            &mut send,
            RichText::new("Нажимать клавиши по нотам").font(theme::semibold(16.0)),
        );
        if send != self.cfg.midi.send_keys {
            self.cfg.midi.send_keys = send;
            self.mark_dirty();
        }
        muted(
            ui,
            "Нота зажата — клавиша зажата. Одна нота = одна клавиша, без макросов. \
             Нажимает только настоящее пианино, экранное ничего не жмёт.",
        );
        self.midi_status_line(ui);

        self.piano_header(ui, "Клавиатура");
        self.draw_piano(ui, true);
        self.two_columns(ui, Self::keys_bindings, Self::keys_side);
    }

    fn keys_side(&mut self, ui: &mut egui::Ui) {
        section(ui, "Переключатель на пианино");
        ui.horizontal(|ui| {
            if self.learn == Learn::WaitToggle {
                let n = self.shared.learned_note.swap(-1, Ordering::Relaxed);
                if n >= 0 {
                    let note = n as u8;
                    self.cfg.midi.toggle_note = notes::note_name(note);
                    // Переключатель не может быть одновременно обычной привязкой.
                    self.cfg.keymap.retain(|k, _| notes::parse_note(k) != Some(note));
                    self.learn = Learn::Idle;
                    self.mark_dirty();
                }
                ui.label(RichText::new("Нажми на пианино клавишу-переключатель (например, самую правую)").color(WARN));
                if ui.button("Отмена").clicked() {
                    self.shared.learn_note.store(false, Ordering::Relaxed);
                    self.learn = Learn::Idle;
                }
                ui.ctx().request_repaint_after(Duration::from_millis(30));
            } else {
                match notes::parse_note(&self.cfg.midi.toggle_note) {
                    Some(n) => {
                        key_chip(ui, &format!("{} ({})", notes::note_name(n), notes::note_name_ru(n)));
                    }
                    None => {
                        ui.label(RichText::new("не назначен").color(MUTED));
                    }
                }
                let label = if self.cfg.midi.toggle_note.is_empty() {
                    "Назначить ноту"
                } else {
                    "Другая нота"
                };
                if ui
                    .add_enabled(self.learn == Learn::Idle, egui::Button::new(label))
                    .clicked()
                {
                    self.shared.learned_note.store(-1, Ordering::Relaxed);
                    self.shared.learn_note.store(true, Ordering::Relaxed);
                    self.learn = Learn::WaitToggle;
                }
                if !self.cfg.midi.toggle_note.is_empty() && ui.button("Убрать").clicked() {
                    self.cfg.midi.toggle_note.clear();
                    self.mark_dirty();
                }
            }
        });
        muted(
            ui,
            "Нажми эту клавишу прямо в игре — клавиши для игр включатся или выключатся. \
             В наушниках прозвучит сигнал: вверх — включено, вниз — выключено. Сама нота не звучит.",
        );

        section(ui, "Правая педаль");
        ui.horizontal(|ui| {
            if self.capturing(CapturePurpose::Pedal) {
                ui.label(RichText::new("Нажми клавишу. Esc — отмена").color(WARN));
            } else {
                if self.cfg.midi.pedal_key.is_empty() {
                    ui.label(RichText::new("только звук").color(MUTED));
                } else {
                    key_chip(ui, &display_key_name(&self.cfg.midi.pedal_key));
                }
                if ui.button("Назначить клавишу").clicked() {
                    self.start_capture(CapturePurpose::Pedal);
                }
                if !self.cfg.midi.pedal_key.is_empty() && ui.button("Убрать").clicked() {
                    self.cfg.midi.pedal_key.clear();
                    self.mark_dirty();
                }
            }
        });
        muted(ui, "Например LShift: в Dota это очередь приказов.");
    }

    fn keys_bindings(&mut self, ui: &mut egui::Ui) {
        section(ui, "Привязки");
        // Добавление привязки.
        match self.learn {
            Learn::WaitToggle => {}
            Learn::Idle => {
                if ui.button("Добавить привязку").clicked() {
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
                    ui.label(RichText::new("Нажми ноту на пианино или кликни клавишу выше").color(WARN));
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
                        RichText::new(format!(
                            "{}: теперь нажми клавишу на клавиатуре",
                            notes::note_name(note)
                        ))
                        .color(WARN),
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
            muted(ui, "Привязок пока нет.");
        } else {
            ui.add_space(4.0);
            egui::Grid::new("keymap")
                .num_columns(3)
                .spacing([18.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    for (n, raw, key) in &entries {
                        let text = format!("{} ({})", notes::note_name(*n), notes::note_name_ru(*n));
                        let color = if self.shared.note_is_down(*n) { SIGNAL } else { TEXT };
                        ui.label(RichText::new(text).color(color));
                        key_chip(ui, &display_key_name(key));
                        if ui.small_button("Удалить").clicked() {
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
    }

    // --- Вкладка «Twitch» -----------------------------------------------------------

    fn twitch_tab(&mut self, ui: &mut egui::Ui) {
        // Награда поймана потоком чата — сохраняем.
        let learned = self.shared.twitch_learned.lock_ok().take();
        if let Some(id) = learned {
            self.cfg.twitch.reward_id = id;
            self.mark_dirty();
        }
        let status = self.shared.twitch_status.lock_ok().clone();

        ui.add_space(8.0);
        let mut enabled = self.cfg.twitch.enabled;
        ui.checkbox(
            &mut enabled,
            RichText::new("Озвучивать сообщения из награды").font(theme::semibold(16.0)),
        );
        if enabled != self.cfg.twitch.enabled {
            self.cfg.twitch.enabled = enabled;
            self.mark_dirty();
        }
        muted(
            ui,
            "Зритель берёт награду за баллы канала и пишет текст — его голосом Windows слышат в Discord, \
             в играх и ты в наушниках. Программа не входит в твой аккаунт Twitch и ничего там не создаёт: \
             она просто читает чат канала, как любой зритель.",
        );

        let enabled = self.cfg.twitch.enabled;
        ui.add_enabled_ui(enabled, |ui| {
            self.two_columns(
                ui,
                |s, ui| s.twitch_left(ui, &status),
                |s, ui| s.twitch_right(ui, &status),
            );
        });

        ui.add_space(10.0);
        muted(
            ui,
            "Для стрима: в OBS оставь свой обычный микрофон, а в Discord и играх выбери Micro Edition — \
             тогда на стриме не будет двойного голоса, а озвучка попадёт туда через звук рабочего стола.",
        );
    }

    fn twitch_left(&mut self, ui: &mut egui::Ui, status: &twitch::TwitchStatus) {
        section(ui, "Канал");
        ui.horizontal(|ui| {
            let mut ch = self.cfg.twitch.channel.clone();
            ui.add(
                egui::TextEdit::singleline(&mut ch)
                    .hint_text("логин, как в twitch.tv/логин")
                    .desired_width(240.0),
            );
            if ch != self.cfg.twitch.channel {
                self.cfg.twitch.channel = ch;
                self.mark_dirty();
            }
            let (color, text) = match &status.state {
                ChatState::Connected => (SIGNAL, format!("читаю чат #{}", status.channel)),
                ChatState::Connecting => (MUTED, "подключаюсь…".to_string()),
                ChatState::Error(e) => (WARN, e.clone()),
                ChatState::Off if self.cfg.twitch.channel.trim().is_empty() => (MUTED, "впиши свой канал".to_string()),
                ChatState::Off => (MUTED, "выключено".to_string()),
            };
            let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.0, color);
            ui.label(RichText::new(text).color(color).size(13.0));
        });

        section(ui, "Награда");
        let learning = self.shared.twitch_learning.load(Ordering::Relaxed);
        if learning {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new(format!(
                        "Жду награду в чате #{}. Возьми её на своём канале.",
                        twitch::normalize_channel(&self.cfg.twitch.channel)
                    ))
                    .color(WARN),
                );
                if ui.button("Отмена").clicked() {
                    self.shared.twitch_learning.store(false, Ordering::Relaxed);
                }
            });
        } else if self.cfg.twitch.reward_id.is_empty() {
            ui.label("1. На Twitch: Панель автора → Баллы канала → Управление наградами → «Добавить свою награду».");
            ui.label("2. Назови её, например, «Сообщение в микрофон» и включи «Требуется ввод текста».");
            ui.label("3. Нажми кнопку ниже и возьми награду на своём канале один раз.");
            let can = status.state == ChatState::Connected;
            if ui
                .add_enabled(can, egui::Button::new("Привязать награду"))
                .on_disabled_hover_text("Сначала впиши канал и дождись «читаю чат»")
                .clicked()
            {
                self.shared.twitch_learning.store(true, Ordering::Relaxed);
            }
        } else {
            ui.horizontal(|ui| {
                ui.label(RichText::new("✓ Награда привязана").color(SIGNAL));
                ui.label(
                    RichText::new(truncate(&self.cfg.twitch.reward_id, 13))
                        .color(MUTED)
                        .size(12.5),
                )
                .on_hover_text(&self.cfg.twitch.reward_id);
                if ui.button("Привязать другую").clicked() {
                    self.shared.twitch_learning.store(true, Ordering::Relaxed);
                }
            });
            muted(ui, "Сообщения из других наград и обычный чат программа не читает.");
        }
    }

    fn twitch_right(&mut self, ui: &mut egui::Ui, status: &twitch::TwitchStatus) {
        section(ui, "Голос");
        egui::Grid::new("tts-grid")
            .num_columns(2)
            .min_col_width(150.0)
            .spacing([12.0, 10.0])
            .show(ui, |ui| {
                ui.label("Голос");
                egui::ComboBox::from_id_salt("voice")
                    .selected_text(if self.cfg.twitch.voice.is_empty() {
                        "Первый русский".to_string()
                    } else {
                        self.cfg.twitch.voice.clone()
                    })
                    .show_ui(ui, |ui| {
                        let mut v = self.cfg.twitch.voice.clone();
                        ui.selectable_value(&mut v, String::new(), "Первый русский");
                        for name in &self.voices {
                            ui.selectable_value(&mut v, name.clone(), name);
                        }
                        if v != self.cfg.twitch.voice {
                            self.cfg.twitch.voice = v;
                            self.mark_dirty();
                        }
                    });
                ui.end_row();

                ui.label("Громкость");
                let mut vol = self.cfg.twitch.volume;
                ui.add(egui::Slider::new(&mut vol, 0..=100).suffix("%"));
                if vol != self.cfg.twitch.volume {
                    self.cfg.twitch.volume = vol;
                    self.mark_dirty();
                }
                ui.end_row();

                ui.label("Длина сообщения");
                let mut len = self.cfg.twitch.max_length;
                ui.add(egui::Slider::new(&mut len, 20..=500).suffix(" симв."));
                if len != self.cfg.twitch.max_length {
                    self.cfg.twitch.max_length = len;
                    self.mark_dirty();
                }
                ui.end_row();
            });

        let mut read_name = self.cfg.twitch.read_name;
        ui.checkbox(&mut read_name, "Читать ник: «Вася пишет: привет»");
        let mut to_mic = self.cfg.twitch.to_mic;
        ui.checkbox(&mut to_mic, "В Discord и игры (виртуальный микрофон)");
        let mut to_hp = self.cfg.twitch.to_headphones;
        ui.checkbox(
            &mut to_hp,
            "В наушники (и на стрим, если OBS захватывает звук рабочего стола)",
        );
        if (read_name, to_mic, to_hp)
            != (
                self.cfg.twitch.read_name,
                self.cfg.twitch.to_mic,
                self.cfg.twitch.to_headphones,
            )
        {
            self.cfg.twitch.read_name = read_name;
            self.cfg.twitch.to_mic = to_mic;
            self.cfg.twitch.to_headphones = to_hp;
            self.mark_dirty();
        }
        if ui.button("Проверить голос").clicked() {
            tts::enqueue(
                &self.shared,
                tts::Job {
                    text: "Привет! Так будут звучать сообщения со стрима.".into(),
                    label: "Проверка голоса".into(),
                },
            );
        }
        if let Some(e) = self.shared.tts_error.lock_ok().clone() {
            banner(ui, LAG, &e, None);
        }

        section(ui, "Очередь");
        let now = self.shared.tts_now.lock_ok().clone();
        let queued = self.shared.tts_queue_len.load(Ordering::Relaxed);
        ui.horizontal(|ui| {
            match &now {
                Some(label) => {
                    ui.label(RichText::new(format!("Говорит: {}", truncate(label, 60))).color(SIGNAL));
                    if ui.button("Пропустить").clicked() {
                        self.shared.tts_skip.store(true, Ordering::Relaxed);
                    }
                }
                None => muted(ui, "Сейчас тихо."),
            }
            if queued > 0 {
                ui.label(RichText::new(format!("В очереди: {queued}")).color(MUTED));
                if ui.button("Очистить").clicked() {
                    self.shared.tts_clear.store(true, Ordering::Relaxed);
                }
            }
        });
        if !status.recent.is_empty() {
            ui.add_space(4.0);
            for r in &status.recent {
                ui.horizontal(|ui| {
                    let (mark, color) = if r.spoken {
                        ("озвучено", SIGNAL)
                    } else {
                        ("другая награда", MUTED)
                    };
                    ui.label(RichText::new(format!("{}: {}", r.user, truncate(&r.text, 60))).color(TEXT));
                    ui.label(RichText::new(mark).color(color).size(12.5));
                });
            }
        }
    }

    // --- Вкладка «Оверлей для OBS» --------------------------------------------------

    fn overlay_tab(&mut self, ui: &mut egui::Ui) {
        self.two_columns(ui, Self::overlay_left, Self::overlay_right);
    }

    fn overlay_left(&mut self, ui: &mut egui::Ui) {
        section(ui, "Оверлей для OBS");
        let mut on = self.cfg.overlay.enabled;
        ui.checkbox(&mut on, "Показывать пианино на стриме");
        if on != self.cfg.overlay.enabled {
            self.cfg.overlay.enabled = on;
            self.mark_dirty();
        }
        let state = self.shared.overlay_state.lock_ok().clone();
        match &state {
            OverlayState::Running(port) => {
                let url = format!("http://localhost:{port}/piano");
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&url).font(theme::semibold(15.0)).color(SIGNAL));
                });
                ui.horizontal(|ui| {
                    if ui.button("Скопировать адрес").clicked() {
                        ui.ctx().copy_text(url.clone());
                    }
                    if ui.button("Открыть в браузере").clicked() {
                        let _ = std::process::Command::new("explorer").arg(&url).spawn();
                    }
                });
            }
            OverlayState::Error(e) => {
                banner(ui, WARN, e, None);
            }
            OverlayState::Off => muted(ui, "Выключено."),
        }

        ui.add_enabled_ui(self.cfg.overlay.enabled, |ui| {
            section(ui, "Какие клавиши показывать");
            let from = notes::parse_note(&self.cfg.overlay.from).unwrap_or(36);
            let to = notes::parse_note(&self.cfg.overlay.to).unwrap_or(96);
            let current = KEYBOARD_SIZES
                .iter()
                .find(|(_, f, t)| *f == from && *t == to)
                .map(|(n, _, _)| n.to_string())
                .unwrap_or_else(|| format!("{}–{}", notes::note_name(from), notes::note_name(to)));
            egui::ComboBox::from_id_salt("kb-size")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (name, f, t) in KEYBOARD_SIZES {
                        if ui.selectable_label(*f == from && *t == to, *name).clicked() {
                            self.cfg.overlay.from = notes::note_name(*f);
                            self.cfg.overlay.to = notes::note_name(*t);
                            self.mark_dirty();
                        }
                    }
                });
            let mut names = self.cfg.overlay.names;
            ui.checkbox(&mut names, "Подписывать нажатые ноты");
            if names != self.cfg.overlay.names {
                self.cfg.overlay.names = names;
                self.mark_dirty();
            }
            ui.horizontal(|ui| {
                ui.label("Порт");
                let mut port = self.cfg.overlay.port;
                ui.add(egui::DragValue::new(&mut port).range(1024..=65535));
                if port != self.cfg.overlay.port {
                    self.cfg.overlay.port = port;
                    self.mark_dirty();
                }
            });
            muted(ui, "Сервер доступен только с этого компьютера.");
        });
    }

    fn overlay_right(&mut self, ui: &mut egui::Ui) {
        section(ui, "Как добавить в OBS");
        ui.label("1. Скопируй адрес слева.");
        ui.label("2. В OBS: Источники → «+» → «Браузер».");
        ui.label("3. Вставь адрес в поле URL.");
        ui.label("4. Ширина 1200, высота 200 — потом растяни как удобно.");
        ui.label("5. Фон уже прозрачный: видно только пианино.");

        section(ui, "Так увидят зрители");
        let from = notes::parse_note(&self.cfg.overlay.from).unwrap_or(36);
        let to = notes::parse_note(&self.cfg.overlay.to).unwrap_or(96);
        self.draw_keyboard_preview(ui, from, to);
        muted(ui, "Нажми клавиши на пианино — они подсветятся и здесь, и в OBS.");
    }

    /// Пианино-превью для вкладки оверлея (без кликов).
    fn draw_keyboard_preview(&self, ui: &mut egui::Ui, from: u8, to: u8) {
        let width = ui.available_width();
        let whites: Vec<u8> = (from..=to).filter(|&n| !notes::is_black(n)).collect();
        if whites.is_empty() {
            return;
        }
        let ww = width / whites.len() as f32;
        let height = (ww * 5.0).clamp(60.0, 110.0);
        let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
        let p = ui.painter_at(rect);
        for (i, &n) in whites.iter().enumerate() {
            let r = Rect::from_min_size(pos2(rect.left() + i as f32 * ww, rect.top()), vec2(ww, height)).shrink(0.5);
            let fill = if self.shared.note_is_down(n) {
                SIGNAL
            } else {
                Color32::from_rgb(0xEC, 0xEF, 0xF4)
            };
            p.rect_filled(
                r,
                CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 3,
                    se: 3,
                },
                fill,
            );
        }
        for n in from..=to {
            if !notes::is_black(n) {
                continue;
            }
            let Some(i) = whites.iter().position(|&w| w == n - 1) else {
                continue;
            };
            let x = rect.left() + (i + 1) as f32 * ww;
            let r = Rect::from_center_size(pos2(x, rect.top() + height * 0.31), vec2(ww * 0.6, height * 0.62));
            let fill = if self.shared.note_is_down(n) {
                SIGNAL
            } else {
                Color32::from_rgb(0x12, 0x16, 0x1F)
            };
            p.rect_filled(
                r,
                CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 2,
                    se: 2,
                },
                fill,
            );
        }
    }

    // --- Вкладка «Устройства» -----------------------------------------------------

    /// Запускает проверку виртуального микрофона в отдельном потоке (~2 секунды).
    fn start_check(&mut self, mic: VirtualMic) {
        {
            let mut st = self.check.lock_ok();
            if matches!(*st, CheckState::Running) {
                return;
            }
            *st = CheckState::Running;
        }
        let state = self.check.clone();
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            // На время проверки программа молчит, чтобы не мешать замеру.
            shared.mute_outputs.store(true, Ordering::Relaxed);
            let result = virtual_mic::loopback_test(&mic.render, &mic.capture).map_err(|e| format!("{e:#}"));
            shared.mute_outputs.store(false, Ordering::Relaxed);
            *state.lock_ok() = CheckState::Done {
                render: mic.render,
                result,
                applied: false,
            };
        });
    }

    /// Записывает результат проверки в настройки и сам проверяет новый кабель.
    fn poll_check(&mut self) {
        let mut measured = None;
        let mut last_render = None;
        let idle;
        {
            let mut guard = self.check.lock_ok();
            idle = matches!(*guard, CheckState::Idle);
            if let CheckState::Done {
                render,
                result,
                applied,
            } = &mut *guard
            {
                last_render = Some(render.clone());
                if !*applied {
                    *applied = true;
                    if let Ok(r) = result {
                        if r.works() {
                            measured = Some((render.clone(), (r.gain_db * 10.0).round() / 10.0));
                        }
                    }
                }
            }
        }
        if let Some((render, gain)) = measured {
            self.cfg.audio.cable_gain_db = gain;
            self.cfg.audio.cable_gain_device = render;
            self.mark_dirty();
            return;
        }
        // Звук пишется в виртуальный кабель, а его громкость ещё не измерена.
        let st = self.shared.status();
        if let (Some(out), Some(cap)) = (st.output_name, st.virtual_capture) {
            let not_checked_yet = idle || last_render.as_deref() != Some(out.as_str());
            if self.cfg.audio.cable_gain_device != out && not_checked_yet {
                let driver = virtual_mic::driver_part(&out).unwrap_or_default().to_string();
                self.start_check(VirtualMic {
                    render: out,
                    capture: cap,
                    driver,
                });
            }
        }
    }

    fn check_result_label(&self, ui: &mut egui::Ui, render: &str) {
        let guard = self.check.lock_ok();
        match &*guard {
            CheckState::Running => {
                ui.spinner();
                ui.label(RichText::new("Проверяю…").color(MUTED));
            }
            CheckState::Done { render: r, result, .. } if r == render => match result {
                Ok(res) if res.works() && res.gain_db.abs() >= 1.0 => {
                    ui.label(
                        RichText::new(format!(
                            "Звук проходит. Кабель усиливал на {:+.0} дБ, громкость выровнена.",
                            res.gain_db
                        ))
                        .color(SIGNAL),
                    );
                }
                Ok(res) if res.works() => {
                    ui.label(RichText::new("Звук проходит, громкость в норме.").color(SIGNAL));
                }
                Ok(_) => {
                    ui.label(
                        RichText::new("Звук через кабель не проходит. Выбери другой или переустанови драйвер.")
                            .color(WARN),
                    );
                }
                Err(e) => {
                    ui.label(RichText::new(format!("Проверка не удалась: {e}")).color(LAG));
                }
            },
            _ if self.cfg.audio.cable_gain_device == render => {
                let g = self.cfg.audio.cable_gain_db;
                let text = if g.abs() >= 1.0 {
                    format!("Проверен. Кабель усиливает на {g:+.0} дБ, программа это выравнивает.")
                } else {
                    "Проверен, громкость в норме.".to_string()
                };
                ui.label(RichText::new(text).color(MUTED));
            }
            _ => {
                ui.label(RichText::new("Ещё не проверялся.").color(MUTED));
            }
        }
    }

    fn devices_tab(&mut self, ui: &mut egui::Ui) {
        self.two_columns(ui, Self::devices_left, Self::devices_right);
    }

    fn devices_left(&mut self, ui: &mut egui::Ui) {
        let mut restart = false;
        let st = self.shared.status();
        let mics = virtual_mic::pair_virtual_mics(&self.inputs, &self.outputs);

        section(ui, "Виртуальный микрофон");
        muted(
            ui,
            "Отдельный микрофон для Discord и игр: в него идёт голос с эффектами и пианино. \
             Твой обычный микрофон остаётся как был, переключаться между ними можно в любой момент.",
        );
        if mics.is_empty() {
            banner(
                ui,
                WARN,
                "Виртуальных микрофонов в системе нет. Поставь VB-Audio Virtual Cable (инструкция ниже).",
                None,
            );
        }
        let mut picked = None;
        for m in &mics {
            let selected = st.output_name.as_deref() == Some(m.render.as_str());
            if virtual_tile(ui, m, selected) && !selected {
                picked = Some(m.clone());
            }
        }
        if let Some(m) = picked {
            self.cfg.audio.output_device = m.render.clone();
            restart = true;
            self.start_check(m);
        }

        if let (Some(out), Some(cap)) = (st.output_name.clone(), st.virtual_capture.clone()) {
            let running = matches!(*self.check.lock_ok(), CheckState::Running);
            if ui.add_enabled(!running, egui::Button::new("Проверить звук")).clicked() {
                let driver = virtual_mic::driver_part(&out).unwrap_or_default().to_string();
                self.start_check(VirtualMic {
                    render: out.clone(),
                    capture: cap.clone(),
                    driver,
                });
            }
            ui.horizontal_wrapped(|ui| self.check_result_label(ui, &out));
            ui.label(
                RichText::new(format!(
                    "В Discord и играх выбери микрофон «{cap}». Шумоподавление Krisp в Discord лучше выключить."
                ))
                .color(TEXT),
            );
            let renamed = virtual_mic::driver_part(&cap) != Some(mic_display_name(&cap).as_str());
            egui::CollapsingHeader::new(RichText::new("Как назвать его «Micro Edition»").font(theme::semibold(15.0)))
                .default_open(!renamed)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("1. Открой настройки звука Windows.");
                        if ui.button("Открыть").clicked() {
                            let _ = std::process::Command::new("explorer").arg("ms-settings:sound").spawn();
                        }
                    });
                    ui.label(format!("2. В разделе «Ввод» нажми на «{cap}»."));
                    ui.label("3. Рядом с названием нажми «Переименовать» и впиши «Micro Edition».");
                    ui.label("4. Здесь нажми «Обновить список устройств». В Discord и играх появится «Micro Edition».");
                });
        } else if let Some(out) = &st.output_name {
            muted(
                ui,
                format!(
                    "Сейчас звук идёт в «{out}». Это не виртуальный микрофон: Discord и игры его не услышат, зато можно послушать себя."
                ),
            );
        }

        if restart {
            self.mark_dirty();
            self.restart_audio();
        }
    }

    fn devices_right(&mut self, ui: &mut egui::Ui) {
        let mut restart = false;
        let headphones_used = self.cfg.audio.monitor
            || (self.cfg.piano.enabled && self.cfg.piano.monitor)
            || (self.cfg.twitch.enabled && self.cfg.twitch.to_headphones);
        section(ui, "Звук");
        egui::Grid::new("devices")
            .num_columns(2)
            .min_col_width(170.0)
            .spacing([12.0, 10.0])
            .show(ui, |ui| {
                ui.label("Микрофон");
                egui::ComboBox::from_id_salt("input")
                    .selected_text(if self.cfg.audio.input_device.is_empty() {
                        "По умолчанию в Windows".to_string()
                    } else {
                        self.cfg.audio.input_device.clone()
                    })
                    .show_ui(ui, |ui| {
                        let mut v = self.cfg.audio.input_device.clone();
                        ui.selectable_value(&mut v, String::new(), "По умолчанию в Windows");
                        for name in &self.inputs {
                            ui.selectable_value(&mut v, name.clone(), name);
                        }
                        if v != self.cfg.audio.input_device {
                            self.cfg.audio.input_device = v;
                            restart = true;
                        }
                    });
                ui.end_row();

                ui.label("Усиление микрофона");
                let mut gain = self.cfg.audio.input_gain_db;
                ui.add(
                    egui::Slider::new(&mut gain, -24.0..=24.0)
                        .suffix(" дБ")
                        .fixed_decimals(1),
                );
                if (gain - self.cfg.audio.input_gain_db).abs() > f32::EPSILON {
                    self.cfg.audio.input_gain_db = gain;
                    self.mark_dirty();
                }
                ui.end_row();

                ui.label("Куда идёт звук");
                egui::ComboBox::from_id_salt("output")
                    .selected_text(if self.cfg.audio.output_device.is_empty() {
                        "Автоматически".to_string()
                    } else {
                        self.cfg.audio.output_device.clone()
                    })
                    .show_ui(ui, |ui| {
                        let mut v = self.cfg.audio.output_device.clone();
                        ui.selectable_value(&mut v, String::new(), "Автоматически (виртуальный микрофон)");
                        for name in &self.outputs {
                            ui.selectable_value(&mut v, name.clone(), name);
                        }
                        if v != self.cfg.audio.output_device {
                            self.cfg.audio.output_device = v;
                            restart = true;
                        }
                    });
                ui.end_row();

                ui.label("Наушники");
                ui.add_enabled_ui(headphones_used, |ui| {
                    egui::ComboBox::from_id_salt("monitor")
                        .selected_text(if self.cfg.audio.monitor_device.is_empty() {
                            "По умолчанию в Windows".to_string()
                        } else {
                            self.cfg.audio.monitor_device.clone()
                        })
                        .show_ui(ui, |ui| {
                            let mut v = self.cfg.audio.monitor_device.clone();
                            ui.selectable_value(&mut v, String::new(), "По умолчанию в Windows");
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

                ui.label("Громкость в наушниках");
                let mut vol = self.cfg.audio.monitor_volume;
                ui.add_enabled(headphones_used, egui::Slider::new(&mut vol, 0..=100).suffix("%"));
                if vol != self.cfg.audio.monitor_volume {
                    self.cfg.audio.monitor_volume = vol;
                    self.mark_dirty();
                }
                ui.end_row();
            });

        let mut monitor = self.cfg.audio.monitor;
        ui.checkbox(&mut monitor, "Слышать себя в наушниках (как тебя слышат в Discord)");
        if monitor != self.cfg.audio.monitor {
            self.cfg.audio.monitor = monitor;
            restart = true;
        }

        if ui.button("Обновить список устройств").clicked() {
            let (i, o) = engine::list_devices();
            self.inputs = i;
            self.outputs = o;
            restart = true;
        }
        if restart {
            self.mark_dirty();
            self.restart_audio();
        }

        // Про отсутствие кабеля уже говорит плашка сверху.
        for w in self.shared.status().warnings.iter().filter(|w| {
            !w.starts_with("Пианино") && !w.starts_with("Не найден выход") && !w.starts_with("Не выбран выход")
        }) {
            banner(ui, WARN, w, None);
        }

        ui.add_space(6.0);
        let open = std::mem::take(&mut self.open_cable_help);
        egui::CollapsingHeader::new(RichText::new("Установить VB-Audio Virtual Cable").font(theme::semibold(16.0)))
            .open(if open { Some(true) } else { None })
            .show(ui, |ui| {
                muted(
                    ui,
                    "Нужен, если виртуальных микрофонов нет или Steam Streaming Microphone чем-то не подходит.",
                );
                ui.horizontal(|ui| {
                    ui.label("1. Скачай VB-CABLE Driver Pack с сайта VB-Audio.");
                    if ui.button("Открыть сайт").clicked() {
                        let _ = std::process::Command::new("explorer").arg(CABLE_SITE).spawn();
                    }
                });
                ui.label("2. Распакуй архив целиком в обычную папку. Из архива установщик не сработает.");
                ui.label(
                    "3. Правой кнопкой по VBCABLE_Setup_x64.exe → «Запуск от имени администратора» → Install Driver.",
                );
                ui.label("4. Перезагрузи компьютер. Программа найдёт кабель сама и покажет его выше.");
            });

        section(ui, "Файл настроек");
        ui.horizontal(|ui| {
            if ui.button("Открыть в Блокноте").clicked() {
                self.save_now();
                let _ = std::process::Command::new("notepad").arg(&self.cfg_path).spawn();
            }
            if ui
                .button("Перечитать")
                .on_hover_text("Применить правки, сделанные в Блокноте")
                .clicked()
            {
                self.reload_config();
            }
        });
    }
}

/// Плитка виртуального микрофона на вкладке «Устройства».
fn virtual_tile(ui: &mut egui::Ui, mic: &VirtualMic, selected: bool) -> bool {
    let width = ui.available_width().min(680.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 56.0), Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let p = ui.painter();
    let fill = if selected {
        SIGNAL_DIM
    } else if resp.hovered() {
        RAISED
    } else {
        PANEL
    };
    p.rect_filled(rect, 8.0, fill);
    p.rect_stroke(
        rect,
        8.0,
        Stroke::new(if selected { 1.5 } else { 1.0 }, if selected { SIGNAL } else { LINE }),
        StrokeKind::Inside,
    );
    // Кружок выбора.
    let c = pos2(rect.left() + 20.0, rect.center().y);
    p.circle_stroke(c, 7.0, Stroke::new(1.5, if selected { SIGNAL } else { MUTED }));
    if selected {
        p.circle_filled(c, 3.5, SIGNAL);
    }
    let note = if mic.driver.to_lowercase().contains("steam") {
        "ставится вместе со Steam"
    } else {
        "драйвер VB-Audio"
    };
    let shown = mic_display_name(&mic.capture);
    let sub = if shown == mic.driver {
        note.to_string()
    } else {
        format!("в Discord и играх: «{shown}», {note}")
    };
    p.text(
        rect.left_top() + vec2(38.0, 9.0),
        Align2::LEFT_TOP,
        &mic.driver,
        theme::semibold(15.0),
        TEXT,
    );
    p.text(
        rect.left_top() + vec2(38.0, 31.0),
        Align2::LEFT_TOP,
        sub,
        theme::regular(12.5),
        MUTED,
    );
    resp.clicked()
}

/// Название клавиши для списка: кнопки мыши — по-русски.
fn display_key_name(name: &str) -> String {
    match name {
        "Mouse1" => "ЛКМ".into(),
        "Mouse2" => "ПКМ".into(),
        "Mouse3" => "Колесо (СКМ)".into(),
        "Mouse4" => "Мышь 4".into(),
        "Mouse5" => "Мышь 5".into(),
        other => other.to_string(),
    }
}

/// Клавиша в «рамке», как на клавиатуре.
fn key_chip(ui: &mut egui::Ui, name: &str) {
    Frame::new()
        .fill(SUNKEN)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(5)
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(name).font(theme::semibold(14.0)).color(TEXT));
        });
}

impl App {
    fn show_window(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.hidden = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }
}

impl eframe::App for App {
    /// Вызывается каждый кадр и когда окно спрятано в трей.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // «Клавиши для игр» переключили нотой на пианино — запоминаем в настройках.
        let live = self.shared.keys_enabled.load(Ordering::Relaxed);
        if live != self.cfg.midi.send_keys {
            self.cfg.midi.send_keys = live;
            self.mark_dirty();
        }
        // Программу запустили ещё раз — показываем окно этой копии.
        if self.shared.show_request.swap(false, Ordering::Relaxed) {
            self.show_window(ctx);
        }
        let lag = self.shared.lag_active();
        let Some(tray) = self.tray.as_mut() else { return };
        tray.set_lag(lag);
        let actions = tray.poll();
        for a in actions {
            match a {
                TrayAction::Show => self.show_window(ctx),
                TrayAction::ToggleLag => {
                    self.shared.lag_latched.fetch_xor(true, Ordering::Relaxed);
                }
                TrayAction::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        // Крестик и «свернуть» прячут окно в трей, как у Discord.
        let (close, minimized) =
            ctx.input(|i| (i.viewport().close_requested(), i.viewport().minimized.unwrap_or(false)));
        if close && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(ctx);
        } else if minimized && !self.hidden {
            self.hide_window(ctx);
        }
        // Значок в трее следит за лагом и когда окно спрятано.
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_capture();
        self.poll_check();

        // Слева — вкладки и состояние, справа — цепочка сигнала и содержимое вкладки.
        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(210.0)
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0, LINE))
                    .inner_margin(Margin {
                        left: 12,
                        right: 12,
                        top: 16,
                        bottom: 14,
                    }),
            )
            .show(ui, |ui| self.sidebar(ui));

        egui::Panel::top("chain")
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(theme::BG).inner_margin(Margin {
                left: 24,
                right: 24,
                top: 16,
                bottom: 4,
            }))
            .show(ui, |ui| {
                self.chain(ui);
                ui.add_space(8.0);
                self.banners(ui);
                let r = ui.max_rect();
                let y = ui.cursor().top() + 2.0;
                ui.painter()
                    .line_segment([pos2(r.left(), y), pos2(r.right(), y)], Stroke::new(1.0, LINE));
                ui.add_space(4.0);
            });

        egui::CentralPanel::default()
            .frame(Frame::new().fill(theme::BG).inner_margin(Margin {
                left: 24,
                right: 24,
                top: 0,
                bottom: 14,
            }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.cfg.ui.tab {
                        Tab::Lag => self.lag_tab(ui),
                        Tab::Effects => self.effects_tab(ui),
                        Tab::Piano => self.piano_tab(ui),
                        Tab::Keys => self.keys_tab(ui),
                        Tab::Twitch => self.twitch_tab(ui),
                        Tab::Overlay => self.overlay_tab(ui),
                        Tab::Devices => self.devices_tab(ui),
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
        // Уровни, подсветка нот и «рваная» линия лага обновляются ~30 раз в секунду.
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }

    fn on_exit(&mut self) {
        // Ни одна клавиша не должна остаться зажатой в игре.
        self.shared.keymapper.lock_ok().release_all();
        if self.dirty_since.is_some() {
            self.save_now();
        }
        let _ = self.engine.send(Cmd::Shutdown);
    }
}
