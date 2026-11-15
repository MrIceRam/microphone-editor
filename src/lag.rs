//! Эффект «лагающего голоса».
//!
//! Как это работает:
//! Discord передаёт голос кодеком Opus пакетами по 20 мс. Когда интернет
//! плохой, часть пакетов теряется или приходит пачкой с опозданием.
//! Декодер Opus «додумывает» потерянные куски (PLC — packet loss
//! concealment) — отсюда металлический роботный голос. А после скачка
//! пинга накопившиеся пакеты проигрываются ускоренно.
//!
//! Здесь всё то же самое, только «плохая сеть» симулируется: голос
//! по-настоящему кодируется в Opus, часть пакетов выкидывается или
//! задерживается, а потом декодируется обратно. Поэтому звучит так же,
//! как у реально лагающего человека.
//!
//! Кодек работает всегда (даже когда лаг выключен), чтобы включение и
//! выключение лага было плавным, без щелчков. Без лага слышен «сухой»
//! голос, задержанный ровно на задержку кодека (6.5 мс).

use std::collections::VecDeque;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Внутренняя частота дискретизации (родная для Opus и Discord).
pub const SAMPLE_RATE: u32 = 48_000;
/// Один пакет = 20 мс = 960 сэмплов, как в Discord.
pub const FRAME: usize = 960;

/// Сколько секунд прошлого звука помнить (для «заедания»).
const HISTORY: usize = SAMPLE_RATE as usize / 2;
/// Плавность включения/выключения эффекта: 10 мс.
const FADE_SAMPLES: f32 = 480.0;
/// Мягкие края у повторяемого кусочка при заедании: 2 мс.
const EDGE_FADE: usize = 96;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LagMode {
    #[default]
    Mix = 0,
    Robot = 1,
    Dropouts = 2,
    Spikes = 3,
    Stutter = 4,
}

impl LagMode {
    pub const ALL: [LagMode; 5] = [
        LagMode::Mix,
        LagMode::Robot,
        LagMode::Dropouts,
        LagMode::Spikes,
        LagMode::Stutter,
    ];

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => LagMode::Robot,
            2 => LagMode::Dropouts,
            3 => LagMode::Spikes,
            4 => LagMode::Stutter,
            _ => LagMode::Mix,
        }
    }

    /// Название для файла настроек.
    pub fn config_name(self) -> &'static str {
        match self {
            LagMode::Mix => "mix",
            LagMode::Robot => "robot",
            LagMode::Dropouts => "dropouts",
            LagMode::Spikes => "spikes",
            LagMode::Stutter => "stutter",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LagMode::Mix => "Плохой интернет",
            LagMode::Robot => "Робот",
            LagMode::Dropouts => "Обрывы",
            LagMode::Spikes => "Скачки пинга",
            LagMode::Stutter => "Заедание",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            LagMode::Mix => "всё вперемешку, как при реально плохой связи",
            LagMode::Robot => "потеря пакетов: металлический роботный голос",
            LagMode::Dropouts => "голос пропадает кусками",
            LagMode::Spikes => "голос замирает, потом догоняет",
            LagMode::Stutter => "«п-п-п-привет»",
        }
    }
}

/// Параметры, которые движок читает перед каждым пакетом.
#[derive(Clone, Copy, Debug)]
pub struct LagParams {
    /// Лаг включён (клавиша зажата / переключатель).
    pub active: bool,
    pub mode: LagMode,
    /// Сила эффекта 0..1.
    pub intensity: f32,
    /// После скачка пинга выкидывать пропущенное вместо ускорения.
    pub catchup_skip: bool,
    /// Во сколько раз ускорять при догоне.
    pub catchup_speed: f32,
    /// Битрейт Opus, бит/с.
    pub bitrate: i32,
}

impl Default for LagParams {
    fn default() -> Self {
        Self {
            active: false,
            mode: LagMode::Mix,
            intensity: 0.6,
            catchup_skip: false,
            catchup_speed: 1.6,
            bitrate: 64_000,
        }
    }
}

/// Числовые настройки «плохой сети» для режима и силы эффекта.
/// Если хочется подкрутить, как звучит конкретный режим, — это здесь.
struct Preset {
    /// Доля потерянных пакетов (0..1).
    loss: f32,
    /// Средняя длина серии потерь подряд, в пакетах (1 пакет = 20 мс).
    burst: f32,
    /// Среднее время между «событиями» (скачок пинга / заедание), в пакетах.
    /// 0 = событий нет.
    event_every: f32,
    /// Доля скачков пинга среди событий (остальное — заедания).
    spike_share: f32,
    /// Длительность скачка пинга, в пакетах (от, до).
    spike_len: (u32, u32),
    /// Длительность заедания, в пакетах (от, до).
    stutter_len: (u32, u32),
    /// Длина повторяемого кусочка при заедании, в сэмплах (от, до).
    stutter_seg: (u32, u32),
}

fn preset(mode: LagMode, intensity: f32) -> Preset {
    let i = intensity.clamp(0.0, 1.0);
    let ms = |v: f32| (v * SAMPLE_RATE as f32 / 1000.0) as u32;
    let none = Preset {
        loss: 0.0,
        burst: 1.0,
        event_every: 0.0,
        spike_share: 0.0,
        spike_len: (1, 1),
        stutter_len: (1, 1),
        stutter_seg: (ms(40.0), ms(40.0)),
    };
    match mode {
        LagMode::Robot => Preset {
            loss: 0.15 + 0.45 * i,
            burst: 1.3 + 1.7 * i,
            ..none
        },
        LagMode::Dropouts => Preset {
            loss: 0.12 + 0.33 * i,
            burst: 6.0 + 14.0 * i,
            ..none
        },
        LagMode::Spikes => Preset {
            loss: 0.02 + 0.04 * i,
            burst: 1.5,
            event_every: 150.0 - 100.0 * i,
            spike_share: 1.0,
            spike_len: (10 + (10.0 * i) as u32, 25 + (40.0 * i) as u32),
            ..none
        },
        LagMode::Stutter => Preset {
            event_every: 60.0 - 40.0 * i,
            spike_share: 0.0,
            stutter_len: (3 + (3.0 * i) as u32, 6 + (10.0 * i) as u32),
            stutter_seg: (ms(40.0), ms(60.0 + 80.0 * i)),
            ..none
        },
        LagMode::Mix => Preset {
            loss: 0.06 + 0.22 * i,
            burst: 1.5 + 1.5 * i,
            event_every: 120.0 - 80.0 * i,
            spike_share: 0.6,
            spike_len: (8 + (8.0 * i) as u32, 20 + (30.0 * i) as u32),
            stutter_len: (3, 6 + (8.0 * i) as u32),
            stutter_seg: (ms(40.0), ms(60.0 + 60.0 * i)),
        },
    }
}

/// Декодирует пакет Opus в `out`. Пустой `data` = пакет потерян (декодер «додумывает»).
fn decode(dec: &mut opus::Decoder, data: &[u8], out: &mut [f32]) {
    let total = out.len();
    match dec.decode_float(data, out, false) {
        Ok(n) if n >= total => {}
        Ok(n) => out[n..].fill(0.0),
        Err(_) => out.fill(0.0),
    }
}

/// Маленький быстрый генератор случайных чисел (xorshift64*).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Случайное число в [0, 1).
    fn f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// Случайное целое в [lo, hi] включительно.
    fn range(&mut self, (lo, hi): (u32, u32)) -> u32 {
        if hi <= lo {
            return lo;
        }
        lo + (self.next_u64() % (hi - lo + 1) as u64) as u32
    }
}

enum FreezeKind {
    /// Пакеты не приходят: декодер «додумывает» (робот, потом тишина).
    Plc,
    /// Повторяется последний кусочек звука (заедание).
    Repeat { seg: Vec<f32>, pos: usize },
}

struct Freeze {
    /// Сколько пакетов ещё «висим».
    left: u32,
    kind: FreezeKind,
    /// После события выкинуть накопившееся (иначе — проиграть ускоренно).
    skip_after: bool,
}

pub struct LagEngine {
    enc: opus::Encoder,
    dec: opus::Decoder,
    bitrate: i32,
    packet: Vec<u8>,

    /// Линия задержки для «сухого» голоса (длина = задержка кодека).
    dry_delay: Vec<f32>,
    dry_pos: usize,
    /// Текущая громкость эффекта 0..1 (плавно едет к 0 или 1).
    wet_gain: f32,

    rng: Rng,
    /// Состояние модели потерь: true = «плохая полоса», пакеты теряются.
    loss_bad: bool,
    /// Через сколько пакетов следующее событие.
    next_event: u32,
    freeze: Option<Freeze>,
    /// Пакеты, «застрявшие в сети» во время скачка пинга (None = потерян).
    backlog: VecDeque<Option<Vec<u8>>>,
    /// Звук, который надо проиграть ускоренно (после скачка пинга).
    catchup: VecDeque<f32>,
    catchup_pos: f64,
    /// Симуляция сети сейчас запущена.
    sim_running: bool,

    /// Последние полсекунды выходного звука эффекта (по кругу).
    history: Vec<f32>,
    history_pos: usize,
}

impl LagEngine {
    pub fn new() -> Result<Self> {
        let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)
            .context("не удалось создать кодировщик Opus")?;
        let bitrate = 64_000;
        enc.set_bitrate(opus::Bitrate::Bits(bitrate))?;
        let lookahead = enc.get_lookahead().unwrap_or(312).max(1) as usize;
        let dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)
            .context("не удалось создать декодер Opus")?;

        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x1234_5678);

        Ok(Self {
            enc,
            dec,
            bitrate,
            packet: vec![0; 4000],
            dry_delay: vec![0.0; lookahead],
            dry_pos: 0,
            wet_gain: 0.0,
            rng: Rng::new(seed),
            loss_bad: false,
            next_event: 0,
            freeze: None,
            backlog: VecDeque::new(),
            catchup: VecDeque::new(),
            catchup_pos: 0.0,
            sim_running: false,
            history: vec![0.0; HISTORY],
            history_pos: 0,
        })
    }

    /// Задержка кодека в сэмплах (на столько же задержан «сухой» голос).
    #[cfg(test)]
    pub fn codec_delay(&self) -> usize {
        self.dry_delay.len()
    }

    /// Насколько сейчас слышен эффект (0..1).
    pub fn wet_gain(&self) -> f32 {
        self.wet_gain
    }

    /// Обрабатывает один пакет: `input` и `out` — ровно по `FRAME` сэмплов.
    pub fn process(&mut self, input: &[f32], out: &mut [f32], p: &LagParams) {
        debug_assert_eq!(input.len(), FRAME);
        debug_assert_eq!(out.len(), FRAME);

        let bitrate = p.bitrate.clamp(6_000, 256_000);
        if bitrate != self.bitrate && self.enc.set_bitrate(opus::Bitrate::Bits(bitrate)).is_ok() {
            self.bitrate = bitrate;
        }

        // 1. Кодируем пакет — как это делает Discord.
        let len = self.enc.encode_float(input, &mut self.packet).unwrap_or(0);

        // 2. «Мокрый» сигнал: через симуляцию плохой сети или просто декодированный.
        let mut wet = [0.0f32; FRAME];
        if p.active && !self.sim_running {
            self.start_sim(p);
        }
        if self.sim_running {
            let pre = preset(p.mode, p.intensity);
            self.simulate(len, &mut wet, &pre, p);
        } else {
            // Лаг выключен: просто декодируем, чтобы декодер был «в курсе»
            // и включение лага прошло без щелчка.
            self.decode_into(Some(len), &mut wet);
        }
        self.push_history(&wet);

        // 3. Смешиваем сухой и мокрый сигнал с плавным переходом.
        let target = if p.active { 1.0 } else { 0.0 };
        let step = 1.0 / FADE_SAMPLES;
        for i in 0..FRAME {
            let dry = self.dry_delay[self.dry_pos];
            self.dry_delay[self.dry_pos] = input[i];
            self.dry_pos = (self.dry_pos + 1) % self.dry_delay.len();

            if self.wet_gain < target {
                self.wet_gain = (self.wet_gain + step).min(1.0);
            } else if self.wet_gain > target {
                self.wet_gain = (self.wet_gain - step).max(0.0);
            }
            let g = self.wet_gain;
            out[i] = dry * (1.0 - g) + wet[i] * g;
        }

        // Эффект полностью затих — сбрасываем «сеть».
        if !p.active && self.wet_gain <= 0.0 && self.sim_running {
            self.stop_sim();
        }
    }

    fn start_sim(&mut self, p: &LagParams) {
        self.sim_running = true;
        self.loss_bad = false;
        self.freeze = None;
        self.backlog.clear();
        self.catchup.clear();
        self.catchup_pos = 0.0;
        // Первое событие — почти сразу, чтобы нажатие кнопки было слышно.
        let pre = preset(p.mode, p.intensity);
        if pre.event_every > 0.0 {
            self.next_event = self.rng.range((0, 6));
        }
        // В «робот»/«обрывы» сразу начинаем с потери пакетов.
        if pre.loss > 0.0 && pre.event_every == 0.0 {
            self.loss_bad = true;
        }
    }

    fn stop_sim(&mut self) {
        self.sim_running = false;
        self.freeze = None;
        self.backlog.clear();
        self.catchup.clear();
        self.catchup_pos = 0.0;
    }

    /// Декодирует пакет из `self.packet[..len]` (или «потерянный» пакет при `None`).
    fn decode_into(&mut self, len: Option<usize>, out: &mut [f32]) {
        let data: &[u8] = match len {
            Some(n) if n > 0 => &self.packet[..n],
            _ => &[],
        };
        decode(&mut self.dec, data, out);
    }

    fn decode_packet(&mut self, pkt: Option<&[u8]>, out: &mut [f32]) {
        decode(&mut self.dec, pkt.unwrap_or(&[]), out);
    }

    /// Модель потерь Гилберта–Эллиотта: потери идут сериями, как в жизни.
    fn packet_lost(&mut self, pre: &Preset) -> bool {
        if pre.loss <= 0.0 {
            self.loss_bad = false;
            return false;
        }
        let loss = pre.loss.min(0.95);
        let p_bad_to_good = 1.0 / pre.burst.max(1.0);
        let p_good_to_bad = (loss * p_bad_to_good / (1.0 - loss)).min(1.0);
        if self.loss_bad {
            if self.rng.f32() < p_bad_to_good {
                self.loss_bad = false;
            }
        } else if self.rng.f32() < p_good_to_bad {
            self.loss_bad = true;
        }
        self.loss_bad
    }

    fn simulate(&mut self, len: usize, wet: &mut [f32; FRAME], pre: &Preset, p: &LagParams) {
        let lost = self.packet_lost(pre) || len == 0;

        // --- Идёт скачок пинга / заедание: пакет «застревает в сети».
        if let Some(mut fr) = self.freeze.take() {
            self.backlog
                .push_back(if lost { None } else { Some(self.packet[..len].to_vec()) });
            match &mut fr.kind {
                FreezeKind::Plc => self.decode_packet(None, wet),
                FreezeKind::Repeat { seg, pos } => {
                    for w in wet.iter_mut() {
                        *w = seg[*pos];
                        *pos = (*pos + 1) % seg.len();
                    }
                }
            }
            fr.left = fr.left.saturating_sub(1);
            if fr.left == 0 {
                self.finish_freeze(fr.skip_after);
                self.schedule_next_event(pre);
            } else {
                self.freeze = Some(fr);
            }
            return;
        }

        // --- Догоняем после скачка: проигрываем накопившееся ускоренно.
        if !self.catchup.is_empty() {
            let mut tmp = [0.0f32; FRAME];
            self.decode_into(if lost { None } else { Some(len) }, &mut tmp);
            self.catchup.extend(tmp.iter());
            self.play_catchup(wet, p.catchup_speed.clamp(1.05, 4.0) as f64);
            return;
        }

        // --- Обычный пакет: пришёл или потерялся.
        self.decode_into(if lost { None } else { Some(len) }, wet);

        if pre.event_every > 0.0 {
            if self.next_event == 0 {
                // История ещё не содержит текущий кадр — добавим его, чтобы
                // заедание повторяло самый свежий звук.
                self.push_history(wet);
                self.start_event(pre, p);
                // Чтобы не записать кадр в историю дважды:
                self.history_pos = (self.history_pos + HISTORY - FRAME) % HISTORY;
            } else {
                self.next_event -= 1;
            }
        }
    }

    fn start_event(&mut self, pre: &Preset, p: &LagParams) {
        let spike = self.rng.f32() < pre.spike_share;
        if spike {
            self.freeze = Some(Freeze {
                left: self.rng.range(pre.spike_len).max(1),
                kind: FreezeKind::Plc,
                skip_after: p.catchup_skip,
            });
        } else {
            let seg_len = (self.rng.range(pre.stutter_seg) as usize).clamp(EDGE_FADE * 4, HISTORY);
            let mut seg = self.last_samples(seg_len);
            // Мягкие края, чтобы на стыках повторов не щёлкало.
            for k in 0..EDGE_FADE {
                let g = k as f32 / EDGE_FADE as f32;
                seg[k] *= g;
                let last = seg_len - 1 - k;
                seg[last] *= g;
            }
            self.freeze = Some(Freeze {
                left: self.rng.range(pre.stutter_len).max(1),
                kind: FreezeKind::Repeat { seg, pos: 0 },
                skip_after: true,
            });
        }
    }

    fn finish_freeze(&mut self, skip: bool) {
        if skip {
            self.backlog.clear();
            return;
        }
        // Все «застрявшие» пакеты пришли разом — декодируем их в очередь догона.
        let mut tmp = [0.0f32; FRAME];
        while let Some(pkt) = self.backlog.pop_front() {
            self.decode_packet(pkt.as_deref(), &mut tmp);
            self.catchup.extend(tmp.iter());
        }
        self.catchup_pos = 0.0;
    }

    fn schedule_next_event(&mut self, pre: &Preset) {
        if pre.event_every > 0.0 {
            let k = 0.4 + 1.2 * self.rng.f32();
            self.next_event = (pre.event_every * k) as u32;
        }
    }

    /// Проигрывает очередь догона ускоренно (с повышением тона — как в Discord).
    fn play_catchup(&mut self, wet: &mut [f32; FRAME], speed: f64) {
        let len = self.catchup.len();
        let avail = len as f64 - self.catchup_pos;
        // Если остаток помещается в этот пакет — дожимаем его целиком.
        let finish_step = avail / FRAME as f64;
        let (step, finish) = if finish_step <= speed {
            (finish_step.max(1.0), true)
        } else {
            (speed, false)
        };

        let mut pos = self.catchup_pos;
        for w in wet.iter_mut() {
            let i = pos as usize;
            let f = (pos - i as f64) as f32;
            let a = self.catchup[i.min(len - 1)];
            let b = self.catchup[(i + 1).min(len - 1)];
            *w = a + (b - a) * f;
            pos += step;
        }

        if finish {
            self.catchup.clear();
            self.catchup_pos = 0.0;
        } else {
            let used = (pos as usize).min(len);
            self.catchup.drain(..used);
            self.catchup_pos = pos - used as f64;
        }
    }

    fn push_history(&mut self, frame: &[f32]) {
        for &s in frame {
            self.history[self.history_pos] = s;
            self.history_pos = (self.history_pos + 1) % HISTORY;
        }
    }

    fn last_samples(&self, n: usize) -> Vec<f32> {
        let n = n.min(HISTORY);
        let start = (self.history_pos + HISTORY - n) % HISTORY;
        (0..n).map(|k| self.history[(start + k) % HISTORY]).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Псевдо-«голос»: гармоники с меняющейся высотой и паузами.
    fn voice_like(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                let f0 = 140.0 + 40.0 * (t * 1.7).sin();
                let env = ((t * 3.0).sin() * 0.5 + 0.5).powf(2.0);
                let mut s = 0.0;
                for h in 1..8 {
                    s += (std::f32::consts::TAU * f0 * h as f32 * t).sin() / h as f32;
                }
                s * env * 0.3
            })
            .collect()
    }

    fn run(mode: LagMode, active_from_frame: usize, frames: usize) -> Vec<f32> {
        let mut eng = LagEngine::new().unwrap();
        let input = voice_like(frames * FRAME);
        let mut out = vec![0.0; frames * FRAME];
        for f in 0..frames {
            let p = LagParams {
                active: f >= active_from_frame,
                mode,
                intensity: 0.7,
                ..LagParams::default()
            };
            let r = f * FRAME..(f + 1) * FRAME;
            eng.process(&input[r.clone()], &mut out[r], &p);
        }
        out
    }

    #[test]
    fn passthrough_when_inactive() {
        let mut eng = LagEngine::new().unwrap();
        let delay = eng.codec_delay();
        let input = voice_like(FRAME * 20);
        let mut out = vec![0.0; FRAME * 20];
        for f in 0..20 {
            let r = f * FRAME..(f + 1) * FRAME;
            eng.process(&input[r.clone()], &mut out[r], &LagParams::default());
        }
        // Без лага — тот же звук, просто задержан на задержку кодека.
        for i in delay..out.len() {
            assert!((out[i] - input[i - delay]).abs() < 1e-6);
        }
    }

    #[test]
    fn decoded_signal_is_aligned_with_dry() {
        // Проверяем, что задержка «сухого» сигнала совпадает с задержкой кодека,
        // иначе при включении лага был бы слышен «двойной» звук.
        let mut eng = LagEngine::new().unwrap();
        let delay = eng.codec_delay() as i64;
        let input = voice_like(FRAME * 50);
        let mut decoded = Vec::new();
        let mut buf = vec![0.0; FRAME];
        for f in 0..50 {
            let len = eng.enc.encode_float(&input[f * FRAME..(f + 1) * FRAME], &mut eng.packet).unwrap();
            eng.decode_into(Some(len), &mut buf);
            decoded.extend_from_slice(&buf);
        }
        let best = (0..700i64)
            .max_by(|&a, &b| {
                let c = |lag: i64| -> f64 {
                    (5000..40000)
                        .map(|i| decoded[i] as f64 * input[(i as i64 - lag) as usize] as f64)
                        .sum()
                };
                c(a).partial_cmp(&c(b)).unwrap()
            })
            .unwrap();
        // Фильтры внутри Opus слегка сдвигают фазу, поэтому пик корреляции может
        // отличаться на несколько сэмплов (8 сэмплов = 0.17 мс — на слух не отличить).
        assert!((best - delay).abs() <= 8, "best lag {best}, codec delay {delay}");
    }

    #[test]
    fn all_modes_produce_sane_audio() {
        for mode in LagMode::ALL {
            let out = run(mode, 10, 400);
            assert!(out.iter().all(|s| s.is_finite()), "{mode:?}: NaN/inf");
            assert!(out.iter().all(|s| s.abs() < 4.0), "{mode:?}: слишком громко");
            // Эффект должен реально менять звук.
            let clean = run(mode, usize::MAX, 400);
            let diff: f32 = out.iter().zip(&clean).map(|(a, b)| (a - b).abs()).sum();
            assert!(diff > 10.0, "{mode:?}: эффект не слышен (diff = {diff})");
        }
    }

    #[test]
    fn effect_turns_off_cleanly() {
        let mut eng = LagEngine::new().unwrap();
        let input = voice_like(FRAME * 300);
        let mut out = vec![0.0; FRAME];
        for f in 0..300 {
            let p = LagParams {
                active: (20..200).contains(&f),
                mode: LagMode::Spikes,
                intensity: 1.0,
                ..LagParams::default()
            };
            eng.process(&input[f * FRAME..(f + 1) * FRAME], &mut out, &p);
        }
        assert_eq!(eng.wet_gain(), 0.0);
        assert!(!eng.sim_running);
        assert!(eng.catchup.is_empty() && eng.backlog.is_empty());
    }
}
