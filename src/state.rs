//! Общее состояние, которое читают/пишут разные потоки:
//! окно, звуковой движок и поток горячих клавиш.
//! Всё на атомиках, чтобы звуковой поток никогда не ждал блокировок.

use std::sync::Mutex;
use std::sync::atomic::{
    AtomicBool, AtomicI16, AtomicI32, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::mpsc::Sender;

use crate::keysend::KeyMapper;
use crate::lag::{LagMode, LagParams};
use crate::midi::MidiStatus;
use crate::overlay::OverlayState;
use crate::tts;
use crate::twitch::TwitchStatus;

/// Блокировка, которая не «ломается», если другой поток упал, держа её.
/// Обычный `lock().unwrap()` в таком случае уронил бы и этот поток —
/// одна ошибка в синтезаторе закрывала бы всю программу.
pub trait LockExt<T> {
    fn lock_ok(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_ok(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// f32, который можно безопасно менять из разных потоков.
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

/// Счётчики для диагностики звука. Звуковые потоки только увеличивают их,
/// а читающий (окно / --diagnose) забирает и обнуляет.
pub struct StreamStats {
    pub callbacks: AtomicU64,
    pub min_frames: AtomicU64,
    pub max_frames: AtomicU64,
    /// Самая большая пауза между вызовами, мкс.
    pub max_gap_us: AtomicU64,
    /// Самое долгое время обработки одного вызова, мкс.
    pub max_work_us: AtomicU64,
    /// Сколько раз буфер опустел посреди звука (= слышимый обрыв).
    pub underruns: AtomicU64,
    /// Вызовы, в которые играла тишина, пока буфер снова наполнялся.
    pub silent_callbacks: AtomicU64,
    /// Минимальная заполненность буфера (сэмплов 48 кГц).
    pub min_fill: AtomicU64,
    /// Пакеты, выброшенные из-за переполнения буфера.
    pub dropped: AtomicU64,
    /// Сбои буфера, о которых сообщила Windows (потерялся кусочек звука).
    pub xruns: AtomicU64,
    /// Всего обрывов с момента запуска (не обнуляется при чтении).
    pub underruns_total: AtomicU64,
    pub sample_rate: AtomicU32,
    pub channels: AtomicU32,
}

impl StreamStats {
    pub fn new() -> Self {
        Self {
            callbacks: AtomicU64::new(0),
            min_frames: AtomicU64::new(u64::MAX),
            max_frames: AtomicU64::new(0),
            max_gap_us: AtomicU64::new(0),
            max_work_us: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            silent_callbacks: AtomicU64::new(0),
            min_fill: AtomicU64::new(u64::MAX),
            dropped: AtomicU64::new(0),
            xruns: AtomicU64::new(0),
            underruns_total: AtomicU64::new(0),
            sample_rate: AtomicU32::new(0),
            channels: AtomicU32::new(0),
        }
    }

    pub fn on_callback(&self, frames: usize, gap_us: u64, work_us: u64) {
        let r = Ordering::Relaxed;
        self.callbacks.fetch_add(1, r);
        self.min_frames.fetch_min(frames as u64, r);
        self.max_frames.fetch_max(frames as u64, r);
        self.max_gap_us.fetch_max(gap_us, r);
        self.max_work_us.fetch_max(work_us, r);
    }

    /// Забирает накопленное и обнуляет (кроме формата и общего числа обрывов).
    pub fn take(&self) -> StatsSnapshot {
        let r = Ordering::Relaxed;
        StatsSnapshot {
            callbacks: self.callbacks.swap(0, r),
            min_frames: self.min_frames.swap(u64::MAX, r),
            max_frames: self.max_frames.swap(0, r),
            max_gap_us: self.max_gap_us.swap(0, r),
            max_work_us: self.max_work_us.swap(0, r),
            underruns: self.underruns.swap(0, r),
            silent_callbacks: self.silent_callbacks.swap(0, r),
            min_fill: self.min_fill.swap(u64::MAX, r),
            dropped: self.dropped.swap(0, r),
            xruns: self.xruns.swap(0, r),
            sample_rate: self.sample_rate.load(r),
            channels: self.channels.load(r),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StatsSnapshot {
    pub callbacks: u64,
    pub min_frames: u64,
    pub max_frames: u64,
    pub max_gap_us: u64,
    pub max_work_us: u64,
    pub underruns: u64,
    pub silent_callbacks: u64,
    pub min_fill: u64,
    pub dropped: u64,
    pub xruns: u64,
    pub sample_rate: u32,
    pub channels: u32,
}

/// Миллисекунды с запуска программы.
pub fn now_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// Значение `captured_vk`, означающее «назначение клавиши отменено».
pub const CAPTURE_CANCELLED: u16 = 0xFFFF;

#[derive(Default, Clone)]
pub struct EngineStatus {
    /// Микрофон открыт и звук обрабатывается.
    pub running: bool,
    /// Главная ошибка (если звук вообще не запустился).
    pub error: Option<String>,
    /// Предупреждения (например, не найден виртуальный кабель).
    pub warnings: Vec<String>,
    pub input_name: String,
    pub output_name: Option<String>,
    /// Если выход — виртуальный кабель: его микрофон (его выбирают в Discord и играх).
    pub virtual_capture: Option<String>,
    pub monitor_name: Option<String>,
    /// Загруженный SoundFont (если пианино со звуком).
    pub soundfont_name: Option<String>,
}

pub struct Shared {
    // --- Лаг ---
    /// Горячая клавиша зажата (режим «удерживать»).
    pub lag_held: AtomicBool,
    /// Лаг включён переключателем (режим «вкл/выкл» или кнопка в окне).
    pub lag_latched: AtomicBool,
    pub lag_mode: AtomicU8,
    /// Сила эффекта 0..1.
    pub intensity: AtomicF32,
    pub catchup_skip: AtomicBool,
    pub catchup_speed: AtomicF32,
    pub bitrate: AtomicI32,

    // --- Звук ---
    /// Усиление микрофона (множитель, не дБ).
    pub input_gain: AtomicF32,
    /// Громкость прослушки 0..1.
    pub monitor_volume: AtomicF32,
    /// Множитель для основного выхода — компенсирует усиление виртуального кабеля.
    pub main_gain: AtomicF32,

    // --- Индикаторы для окна ---
    pub in_level: AtomicF32,
    pub out_level: AtomicF32,
    /// Насколько сейчас слышен эффект (0..1).
    pub lag_wet: AtomicF32,

    // --- Горячая клавиша ---
    pub hotkey_vk: AtomicU16,
    pub hotkey_toggle: AtomicBool,
    /// Окно попросило «нажми клавишу для назначения».
    pub capturing: AtomicBool,
    /// Пойманная клавиша (0 — ещё не поймана).
    pub captured_vk: AtomicU16,

    // --- Движок ---
    pub status: Mutex<EngineStatus>,
    /// Звуковое устройство отвалилось — движок переподключится.
    pub stream_failed: AtomicBool,

    // --- Диагностика ---
    pub input_stats: StreamStats,
    pub main_stats: StreamStats,
    pub monitor_stats: StreamStats,
    /// Выходы пишут тишину (для диагностики, чтобы никто не слышал звук).
    pub mute_outputs: AtomicBool,
    /// Когда звук последний раз запускался (мс с запуска программы).
    pub stream_start_ms: AtomicU64,

    // --- Пианино ---
    /// Очередь MIDI-сообщений в синтезатор (создаётся звуковым движком).
    pub piano_tx: Mutex<Option<rtrb::Producer<[u8; 3]>>>,
    pub piano_enabled: AtomicBool,
    /// Громкость пианино (множитель).
    pub piano_volume: AtomicF32,
    pub piano_program: AtomicU8,
    pub piano_through_lag: AtomicBool,
    /// Сдвиг нот в полутонах.
    pub piano_transpose: AtomicI32,
    /// В наушниках слышен свой голос (весь микс, как в Discord).
    pub monitor_voice: AtomicBool,
    /// В наушниках слышно пианино.
    pub monitor_piano: AtomicBool,

    // --- MIDI ---
    pub midi_device: Mutex<String>,
    pub midi_status: Mutex<MidiStatus>,
    /// Какие ноты сейчас зажаты (128 бит).
    notes_down: [AtomicU64; 2],
    pub pedal_down: AtomicBool,
    pub last_note: AtomicI16,
    pub last_velocity: AtomicU8,
    /// Окно ждёт ноту для новой привязки.
    pub learn_note: AtomicBool,
    /// Пойманная нота (-1 — ещё нет).
    pub learned_note: AtomicI16,
    pub keymapper: Mutex<KeyMapper>,
    /// «Клавиши для игр» включены (меняется и из окна, и нотой-переключателем).
    pub keys_enabled: AtomicBool,
    /// Нота-переключатель «клавиши для игр» (-1 — нет).
    pub toggle_note: AtomicI16,
    /// Звуковой сигнал в наушники: 1 — «включено», 2 — «выключено».
    pub cue: AtomicU8,
    /// Синтезатор пианино отключён из-за ошибки.
    pub piano_failed: AtomicBool,

    // --- Twitch ---
    pub twitch_enabled: AtomicBool,
    pub twitch_channel: Mutex<String>,
    /// ID награды, сообщения из которой озвучиваются.
    pub twitch_reward: Mutex<String>,
    /// Окно ждёт награду для привязки.
    pub twitch_learning: AtomicBool,
    /// Пойманная награда (окно забирает и сохраняет в настройки).
    pub twitch_learned: Mutex<Option<String>>,
    pub twitch_status: Mutex<TwitchStatus>,

    // --- Озвучка ---
    pub tts_jobs: Mutex<Option<Sender<tts::Job>>>,
    /// Звук озвучки для звукового движка (создаётся движком).
    pub tts_tx: Mutex<Option<rtrb::Producer<f32>>>,
    pub tts_queue_len: AtomicUsize,
    /// Пропустить текущее сообщение.
    pub tts_skip: AtomicBool,
    /// Звуковому движку: выкинуть то, что уже в буфере.
    pub tts_flush: AtomicBool,
    /// Очистить очередь.
    pub tts_clear: AtomicBool,
    /// Что сейчас озвучивается.
    pub tts_now: Mutex<Option<String>>,
    pub tts_error: Mutex<Option<String>>,
    pub tts_voice: Mutex<String>,
    pub tts_read_name: AtomicBool,
    pub tts_max_len: AtomicU32,
    pub tts_volume: AtomicF32,
    pub tts_to_mic: AtomicBool,
    pub tts_to_headphones: AtomicBool,

    // --- Эффекты голоса ---
    pub voice_fx: AtomicU8,
    /// Сила эффекта 0..1.
    pub fx_mix: AtomicF32,
    pub denoise: AtomicBool,
    /// Сила шумоподавления 0..1.
    pub denoise_amount: AtomicF32,

    // --- Оверлей для OBS ---
    pub overlay_enabled: AtomicBool,
    pub overlay_port: AtomicU32,
    pub overlay_from: AtomicU8,
    pub overlay_to: AtomicU8,
    pub overlay_names: AtomicBool,
    pub overlay_state: Mutex<OverlayState>,

    // --- Окно ---
    /// Повторный запуск программы просит показать окно из трея.
    pub show_request: AtomicBool,
    /// Контекст окна — чтобы будить его из других потоков.
    pub egui_ctx: std::sync::OnceLock<eframe::egui::Context>,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            lag_held: AtomicBool::new(false),
            lag_latched: AtomicBool::new(false),
            lag_mode: AtomicU8::new(LagMode::Mix as u8),
            intensity: AtomicF32::new(0.6),
            catchup_skip: AtomicBool::new(false),
            catchup_speed: AtomicF32::new(1.6),
            bitrate: AtomicI32::new(64_000),
            input_gain: AtomicF32::new(1.0),
            monitor_volume: AtomicF32::new(0.8),
            main_gain: AtomicF32::new(1.0),
            in_level: AtomicF32::new(0.0),
            out_level: AtomicF32::new(0.0),
            lag_wet: AtomicF32::new(0.0),
            hotkey_vk: AtomicU16::new(0),
            hotkey_toggle: AtomicBool::new(false),
            capturing: AtomicBool::new(false),
            captured_vk: AtomicU16::new(0),
            status: Mutex::new(EngineStatus::default()),
            stream_failed: AtomicBool::new(false),
            input_stats: StreamStats::new(),
            main_stats: StreamStats::new(),
            monitor_stats: StreamStats::new(),
            mute_outputs: AtomicBool::new(false),
            stream_start_ms: AtomicU64::new(0),
            piano_tx: Mutex::new(None),
            piano_enabled: AtomicBool::new(true),
            piano_volume: AtomicF32::new(0.7),
            piano_program: AtomicU8::new(0),
            piano_through_lag: AtomicBool::new(false),
            piano_transpose: AtomicI32::new(0),
            monitor_voice: AtomicBool::new(false),
            monitor_piano: AtomicBool::new(true),
            midi_device: Mutex::new(String::new()),
            midi_status: Mutex::new(MidiStatus::default()),
            notes_down: [AtomicU64::new(0), AtomicU64::new(0)],
            pedal_down: AtomicBool::new(false),
            last_note: AtomicI16::new(-1),
            last_velocity: AtomicU8::new(0),
            learn_note: AtomicBool::new(false),
            learned_note: AtomicI16::new(-1),
            keymapper: Mutex::new(KeyMapper::new()),
            keys_enabled: AtomicBool::new(false),
            toggle_note: AtomicI16::new(-1),
            cue: AtomicU8::new(0),
            piano_failed: AtomicBool::new(false),
            twitch_enabled: AtomicBool::new(false),
            twitch_channel: Mutex::new(String::new()),
            twitch_reward: Mutex::new(String::new()),
            twitch_learning: AtomicBool::new(false),
            twitch_learned: Mutex::new(None),
            twitch_status: Mutex::new(TwitchStatus::default()),
            tts_jobs: Mutex::new(None),
            tts_tx: Mutex::new(None),
            tts_queue_len: AtomicUsize::new(0),
            tts_skip: AtomicBool::new(false),
            tts_flush: AtomicBool::new(false),
            tts_clear: AtomicBool::new(false),
            tts_now: Mutex::new(None),
            tts_error: Mutex::new(None),
            tts_voice: Mutex::new(String::new()),
            tts_read_name: AtomicBool::new(true),
            tts_max_len: AtomicU32::new(200),
            tts_volume: AtomicF32::new(0.8),
            tts_to_mic: AtomicBool::new(true),
            tts_to_headphones: AtomicBool::new(true),
            voice_fx: AtomicU8::new(0),
            fx_mix: AtomicF32::new(1.0),
            denoise: AtomicBool::new(false),
            denoise_amount: AtomicF32::new(1.0),
            overlay_enabled: AtomicBool::new(false),
            overlay_port: AtomicU32::new(7878),
            overlay_from: AtomicU8::new(36),
            overlay_to: AtomicU8::new(96),
            overlay_names: AtomicBool::new(false),
            overlay_state: Mutex::new(OverlayState::Off),
            show_request: AtomicBool::new(false),
            egui_ctx: std::sync::OnceLock::new(),
        }
    }

    /// Первая секунда после запуска звука: устройства «разгоняются», единичные
    /// сбои в это время нормальны и в общий счётчик не идут.
    pub fn in_warmup(&self) -> bool {
        now_ms().saturating_sub(self.stream_start_ms.load(Ordering::Relaxed)) < 1000
    }

    pub fn set_note_down(&self, note: u8, down: bool) {
        let note = note & 127;
        let bit = 1u64 << (note % 64);
        let word = &self.notes_down[(note / 64) as usize];
        if down {
            word.fetch_or(bit, Ordering::Relaxed);
        } else {
            word.fetch_and(!bit, Ordering::Relaxed);
        }
    }

    pub fn note_is_down(&self, note: u8) -> bool {
        let note = note & 127;
        self.notes_down[(note / 64) as usize].load(Ordering::Relaxed) & (1u64 << (note % 64)) != 0
    }

    pub fn clear_notes(&self) {
        for w in &self.notes_down {
            w.store(0, Ordering::Relaxed);
        }
        self.pedal_down.store(false, Ordering::Relaxed);
    }

    pub fn lag_active(&self) -> bool {
        self.lag_held.load(Ordering::Relaxed) || self.lag_latched.load(Ordering::Relaxed)
    }

    pub fn lag_params(&self) -> LagParams {
        LagParams {
            active: self.lag_active(),
            mode: LagMode::from_u8(self.lag_mode.load(Ordering::Relaxed)),
            intensity: self.intensity.get(),
            catchup_skip: self.catchup_skip.load(Ordering::Relaxed),
            catchup_speed: self.catchup_speed.get(),
            bitrate: self.bitrate.load(Ordering::Relaxed),
        }
    }

    pub fn set_status(&self, status: EngineStatus) {
        *self.status.lock_ok() = status;
    }

    pub fn status(&self) -> EngineStatus {
        self.status.lock_ok().clone()
    }
}
