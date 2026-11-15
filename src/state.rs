//! Общее состояние, которое читают/пишут разные потоки:
//! окно, звуковой движок и поток горячих клавиш.
//! Всё на атомиках, чтобы звуковой поток никогда не ждал блокировок.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

use crate::lag::{LagMode, LagParams};

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
    pub monitor_name: Option<String>,
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
        }
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
        *self.status.lock().unwrap() = status;
    }

    pub fn status(&self) -> EngineStatus {
        self.status.lock().unwrap().clone()
    }
}
