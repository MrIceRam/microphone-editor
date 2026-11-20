//! Эффекты голоса и шумоподавление.
//!
//! Порядок обработки голоса: шумоподавление → эффект → лаг.
//! Пианино и озвучка Twitch эффектами не обрабатываются.

use nnnoiseless::DenoiseState;
use serde::Deserialize;

use crate::lag::{FRAME, SAMPLE_RATE};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VoiceFx {
    #[default]
    None = 0,
    Radio = 1,
    Phone = 2,
    Cave = 3,
    Low = 4,
    High = 5,
    Robot = 6,
    Hamster = 7,
}

impl VoiceFx {
    pub const ALL: [VoiceFx; 8] = [
        VoiceFx::None,
        VoiceFx::Radio,
        VoiceFx::Phone,
        VoiceFx::Cave,
        VoiceFx::Low,
        VoiceFx::High,
        VoiceFx::Robot,
        VoiceFx::Hamster,
    ];

    pub fn from_u8(v: u8) -> Self {
        Self::ALL.get(v as usize).copied().unwrap_or(VoiceFx::None)
    }

    pub fn config_name(self) -> &'static str {
        match self {
            VoiceFx::None => "none",
            VoiceFx::Radio => "radio",
            VoiceFx::Phone => "phone",
            VoiceFx::Cave => "cave",
            VoiceFx::Low => "low",
            VoiceFx::High => "high",
            VoiceFx::Robot => "robot",
            VoiceFx::Hamster => "hamster",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            VoiceFx::None => "Без эффекта",
            VoiceFx::Radio => "Рация",
            VoiceFx::Phone => "Телефон",
            VoiceFx::Cave => "Пещера",
            VoiceFx::Low => "Низкий голос",
            VoiceFx::High => "Высокий голос",
            VoiceFx::Robot => "Робот",
            VoiceFx::Hamster => "Хомяк",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            VoiceFx::None => "обычный голос",
            VoiceFx::Radio => "хрип и шипение эфира",
            VoiceFx::Phone => "узкий звук трубки",
            VoiceFx::Cave => "гулкое эхо",
            VoiceFx::Low => "на квинту ниже",
            VoiceFx::High => "на квинту выше",
            VoiceFx::Robot => "металлический гул",
            VoiceFx::Hamster => "пищит на октаву выше",
        }
    }
}

/// Мягкий ограничитель: до 0.8 не трогает, дальше плавно прижимает к 1.0.
fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a < 0.8 { x } else { x.signum() * (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh()) }
}

/// Двухполюсный фильтр (биквад, формулы RBJ).
#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn new(kind: char, freq: f32, q: f32) -> Self {
        let w = std::f32::consts::TAU * freq / SAMPLE_RATE as f32;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        let (b0, b1, b2) = match kind {
            'l' => ((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0),
            _ => ((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0),
        };
        let a0 = 1.0 + alpha;
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: -2.0 * c / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Сдвиг высоты голоса без изменения скорости: две «читающие головки» по
/// линии задержки, которые едут с другой скоростью и плавно сменяют друг друга.
struct PitchShifter {
    buf: Vec<f32>,
    w: usize,
    phase: f32,
}

const PS_BUF: usize = 8192;
/// Окно 30 мс: короче — «булькает», длиннее — заметнее задержка и эхо.
const PS_WINDOW: f32 = 1440.0;

impl PitchShifter {
    fn new() -> Self {
        Self {
            buf: vec![0.0; PS_BUF],
            w: 0,
            phase: 0.0,
        }
    }

    fn read(&self, delay: f32) -> f32 {
        let pos = self.w as f32 - delay;
        let pos = pos.rem_euclid(PS_BUF as f32);
        let i = pos as usize;
        let f = pos - i as f32;
        let a = self.buf[i % PS_BUF];
        let b = self.buf[(i + 1) % PS_BUF];
        a + (b - a) * f
    }

    fn run(&mut self, x: f32, ratio: f32) -> f32 {
        self.buf[self.w] = x;
        self.phase = (self.phase + (1.0 - ratio) / PS_WINDOW).rem_euclid(1.0);
        let p2 = (self.phase + 0.5) % 1.0;
        let s1 = self.read(1.0 + self.phase * PS_WINDOW);
        let s2 = self.read(1.0 + p2 * PS_WINDOW);
        // Треугольные окна в сумме дают 1 — громкость не «качается».
        let g1 = 1.0 - (2.0 * self.phase - 1.0).abs();
        let g2 = 1.0 - (2.0 * p2 - 1.0).abs();
        self.w = (self.w + 1) % PS_BUF;
        s1 * g1 + s2 * g2
    }
}

/// Гребенчатый фильтр с затуханием высоких частот (основа реверберации).
struct Comb {
    buf: Vec<f32>,
    i: usize,
    store: f32,
}

impl Comb {
    fn new(len: usize) -> Self {
        Self {
            buf: vec![0.0; len],
            i: 0,
            store: 0.0,
        }
    }
    fn run(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let out = self.buf[self.i];
        self.store = out * (1.0 - damp) + self.store * damp;
        self.buf[self.i] = x + self.store * feedback;
        self.i = (self.i + 1) % self.buf.len();
        out
    }
}

struct Allpass {
    buf: Vec<f32>,
    i: usize,
}

impl Allpass {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len], i: 0 }
    }
    fn run(&mut self, x: f32) -> f32 {
        let b = self.buf[self.i];
        let out = b - x;
        self.buf[self.i] = x + b * 0.5;
        self.i = (self.i + 1) % self.buf.len();
        out
    }
}

/// Шумоподавление (нейросеть RNNoise, работает без интернета).
pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    out: [f32; DenoiseState::FRAME_SIZE],
    first: bool,
}

impl Denoiser {
    pub fn new() -> Self {
        Self {
            state: DenoiseState::new(),
            out: [0.0; DenoiseState::FRAME_SIZE],
            first: true,
        }
    }

    /// Чистит пакет на месте. `amount` 0..1 — сколько очищенного звука подмешать.
    pub fn process(&mut self, frame: &mut [f32; FRAME], amount: f32) {
        const N: usize = DenoiseState::FRAME_SIZE;
        let mut input = [0.0f32; N];
        for chunk in frame.chunks_exact_mut(N) {
            for (d, s) in input.iter_mut().zip(chunk.iter()) {
                *d = s * 32768.0;
            }
            self.state.process_frame(&mut self.out, &input);
            if self.first {
                // Первый кусок — с артефактами разгона, его не выпускаем.
                self.first = false;
                chunk.fill(0.0);
                continue;
            }
            for (s, o) in chunk.iter_mut().zip(self.out.iter()) {
                *s = *s * (1.0 - amount) + (o / 32768.0) * amount;
            }
        }
    }
}

/// Все эффекты голоса со своим состоянием.
pub struct FxEngine {
    current: VoiceFx,
    hp: Biquad,
    lp: Biquad,
    hp2: Biquad,
    lp2: Biquad,
    pitch: PitchShifter,
    combs: Vec<Comb>,
    allpasses: Vec<Allpass>,
    echo: Vec<f32>,
    echo_i: usize,
    phase: f32,
    rng: u32,
}

impl FxEngine {
    pub fn new() -> Self {
        // Длины гребёнок как у Freeverb, растянутые вдвое — «большая пещера».
        let combs = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617]
            .iter()
            .map(|&l| Comb::new(l * 2))
            .collect();
        let allpasses = [556, 441, 341, 225].iter().map(|&l| Allpass::new(l)).collect();
        let mut fx = Self {
            current: VoiceFx::None,
            hp: Biquad::new('h', 400.0, 0.7),
            lp: Biquad::new('l', 2800.0, 0.7),
            hp2: Biquad::new('h', 400.0, 0.7),
            lp2: Biquad::new('l', 2800.0, 0.7),
            pitch: PitchShifter::new(),
            combs,
            allpasses,
            echo: vec![0.0; SAMPLE_RATE as usize],
            echo_i: 0,
            phase: 0.0,
            rng: 0x1234_5678,
        };
        fx.configure(VoiceFx::None);
        fx
    }

    fn configure(&mut self, fx: VoiceFx) {
        self.current = fx;
        let (hp, lp) = match fx {
            VoiceFx::Radio => (450.0, 2600.0),
            VoiceFx::Phone => (300.0, 3400.0),
            _ => (40.0, 16000.0),
        };
        self.hp = Biquad::new('h', hp, 0.7);
        self.hp2 = Biquad::new('h', hp, 0.7);
        self.lp = Biquad::new('l', lp, 0.7);
        self.lp2 = Biquad::new('l', lp, 0.7);
    }

    fn noise(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// Обрабатывает пакет на месте. `mix` 0..1 — сила эффекта.
    pub fn process(&mut self, frame: &mut [f32; FRAME], fx: VoiceFx, mix: f32) {
        if fx != self.current {
            self.configure(fx);
        }
        if fx == VoiceFx::None || mix <= 0.0 {
            return;
        }
        for s in frame.iter_mut() {
            let dry = *s;
            let wet = match fx {
                VoiceFx::None => dry,
                VoiceFx::Radio => {
                    let x = self.lp2.run(self.lp.run(self.hp2.run(self.hp.run(dry))));
                    let driven = (x * 6.0).tanh() * 0.45;
                    driven + self.noise() * 0.006
                }
                VoiceFx::Phone => {
                    let x = self.lp2.run(self.lp.run(self.hp2.run(self.hp.run(dry))));
                    (x * 2.5).tanh() * 0.6
                }
                VoiceFx::Cave => {
                    // Эхо через 280 мс + густая реверберация.
                    let delay = (0.28 * SAMPLE_RATE as f32) as usize;
                    let len = self.echo.len();
                    let echoed = self.echo[(self.echo_i + len - delay) % len];
                    self.echo[self.echo_i] = dry + echoed * 0.3;
                    self.echo_i = (self.echo_i + 1) % len;
                    let input = (dry + echoed * 0.5) * 0.035;
                    let mut acc = 0.0;
                    for c in self.combs.iter_mut() {
                        acc += c.run(input, 0.86, 0.35);
                    }
                    for a in self.allpasses.iter_mut() {
                        acc = a.run(acc);
                    }
                    soft_limit(dry * 0.7 + acc)
                }
                VoiceFx::Low => self.pitch.run(dry, 0.75) * 1.1,
                VoiceFx::High => self.pitch.run(dry, 1.33),
                VoiceFx::Hamster => self.pitch.run(dry, 1.9),
                VoiceFx::Robot => {
                    // Кольцевая модуляция 50 Гц + металлическая гребёнка 5 мс.
                    self.phase = (self.phase + 50.0 / SAMPLE_RATE as f32) % 1.0;
                    let ring = dry * (self.phase * std::f32::consts::TAU).sin();
                    let len = self.echo.len();
                    let d = 240;
                    let back = self.echo[(self.echo_i + len - d) % len];
                    let y = ring + back * 0.55;
                    self.echo[self.echo_i] = y;
                    self.echo_i = (self.echo_i + 1) % len;
                    soft_limit(y * 1.4)
                }
            };
            *s = dry * (1.0 - mix) + wet * mix;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32) -> Vec<f32> {
        (0..FRAME * 50)
            .map(|i| (std::f32::consts::TAU * freq * i as f32 / SAMPLE_RATE as f32).sin() * 0.3)
            .collect()
    }

    /// Частота по числу пересечений нуля.
    fn estimate_freq(x: &[f32]) -> f32 {
        let crossings = x.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        crossings as f32 * SAMPLE_RATE as f32 / x.len() as f32
    }

    #[test]
    fn pitch_shift_changes_frequency() {
        for (fx, ratio) in [(VoiceFx::Low, 0.75), (VoiceFx::High, 1.33), (VoiceFx::Hamster, 1.9)] {
            let mut eng = FxEngine::new();
            let input = tone(220.0);
            let mut out = Vec::new();
            for chunk in input.chunks_exact(FRAME) {
                let mut f = [0.0f32; FRAME];
                f.copy_from_slice(chunk);
                eng.process(&mut f, fx, 1.0);
                out.extend_from_slice(&f);
            }
            let f = estimate_freq(&out[FRAME * 10..]);
            let want = 220.0 * ratio;
            assert!((f - want).abs() / want < 0.08, "{fx:?}: {f} Гц, ждали {want}");
        }
    }

    #[test]
    fn all_effects_are_finite_and_not_too_loud() {
        for fx in VoiceFx::ALL {
            let mut eng = FxEngine::new();
            let input = tone(300.0);
            let mut peak = 0.0f32;
            for chunk in input.chunks_exact(FRAME) {
                let mut f = [0.0f32; FRAME];
                f.copy_from_slice(chunk);
                eng.process(&mut f, fx, 1.0);
                assert!(f.iter().all(|s| s.is_finite()), "{fx:?}");
                peak = f.iter().fold(peak, |m, s| m.max(s.abs()));
            }
            assert!(peak < 1.5, "{fx:?}: пик {peak}");
            assert!(peak > 0.01, "{fx:?}: тишина");
        }
    }

    #[test]
    fn denoiser_reduces_steady_noise() {
        let mut d = Denoiser::new();
        let mut rng = 1u32;
        let mut energy_in = 0.0;
        let mut energy_out = 0.0;
        for k in 0..100 {
            let mut f = [0.0f32; FRAME];
            for s in f.iter_mut() {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                *s = ((rng as f32 / u32::MAX as f32) * 2.0 - 1.0) * 0.05;
            }
            if k > 20 {
                energy_in += f.iter().map(|s| s * s).sum::<f32>();
            }
            d.process(&mut f, 1.0);
            if k > 20 {
                energy_out += f.iter().map(|s| s * s).sum::<f32>();
            }
        }
        assert!(energy_out < energy_in * 0.25, "шум почти не убрался: {energy_out} из {energy_in}");
    }
}
