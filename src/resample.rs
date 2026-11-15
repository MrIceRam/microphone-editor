//! Простые ресемплеры с линейной интерполяцией.
//!
//! Внутри программа работает на 48 кГц моно (родная частота Opus и Discord).
//! Микрофон и выходы могут работать на другой частоте (44.1 кГц и т.п.),
//! поэтому звук пересчитывается на входе и на выходе.
//! Для голоса линейной интерполяции достаточно; при одинаковых частотах
//! звук проходит без изменений.

use rtrb::Consumer;

/// Ресемплер «на вход»: получает куски звука с частотой `in_rate`
/// и выдаёт звук с частотой `out_rate` (фиксированное соотношение).
pub struct PushResampler {
    /// Сколько входных сэмплов приходится на один выходной.
    step: f64,
    /// Позиция следующего выходного сэмпла между `prev` (0.0) и новым (1.0).
    pos: f64,
    prev: f32,
}

impl PushResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            step: in_rate as f64 / out_rate as f64,
            pos: 0.0,
            prev: 0.0,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for &x in input {
            while self.pos < 1.0 {
                let t = self.pos as f32;
                out.push(self.prev + (x - self.prev) * t);
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.prev = x;
        }
    }
}

/// Ресемплер «на выход»: сам забирает сэмплы 48 кГц из кольцевого буфера
/// и выдаёт их с частотой устройства. Заодно компенсирует разницу хода
/// часов у разных звуковых устройств: если буфер копится — читает чуть
/// быстрее, если пустеет — чуть медленнее. Так не накапливается задержка
/// и не бывает щелчков от переполнения.
pub struct PullResampler {
    base_step: f64,
    pos: f64,
    prev: f32,
    cur: f32,
    /// Сколько сэмплов держать в буфере (в сэмплах 48 кГц).
    target_fill: f64,
    /// Сглаженная заполненность буфера.
    avg_fill: f64,
    /// Буфер набрал нужный запас и можно играть.
    primed: bool,
}

impl PullResampler {
    pub fn new(src_rate: u32, dev_rate: u32, target_fill: usize) -> Self {
        Self {
            base_step: src_rate as f64 / dev_rate as f64,
            pos: 0.0,
            prev: 0.0,
            cur: 0.0,
            target_fill: target_fill as f64,
            avg_fill: target_fill as f64,
            primed: false,
        }
    }

    /// Заполняет `out` (моно, частота устройства) сэмплами из `src`.
    pub fn fill(&mut self, src: &mut Consumer<f32>, out: &mut [f32]) {
        let fill = src.slots() as f64;

        if !self.primed {
            if fill >= self.target_fill {
                self.primed = true;
                self.avg_fill = fill;
            } else {
                out.fill(0.0);
                return;
            }
        }

        // Если буфер раздуло (например, устройство подвисло) — выкидываем лишнее.
        let hard_limit = self.target_fill * 3.0 + 4800.0;
        if fill > hard_limit {
            let excess = (fill - self.target_fill) as usize;
            if let Ok(chunk) = src.read_chunk(excess) {
                chunk.commit_all();
            }
            self.avg_fill = self.target_fill;
        }

        // Плавная подстройка скорости чтения (±0.5% максимум — на слух незаметно).
        let fill = src.slots() as f64;
        self.avg_fill += (fill - self.avg_fill) * 0.05;
        let error_sec = (self.avg_fill - self.target_fill) / 48_000.0;
        let correction = (error_sec * 0.1).clamp(-0.005, 0.005);
        let step = self.base_step * (1.0 + correction);

        for o in out.iter_mut() {
            while self.pos >= 1.0 {
                match src.pop() {
                    Ok(s) => {
                        self.prev = self.cur;
                        self.cur = s;
                        self.pos -= 1.0;
                    }
                    Err(_) => {
                        // Буфер опустел — ждём, пока снова наберётся запас.
                        self.primed = false;
                        self.pos = 1.0;
                        break;
                    }
                }
            }
            if !self.primed {
                *o = 0.0;
                continue;
            }
            let t = self.pos as f32;
            *o = self.prev + (self.cur - self.prev) * t;
            self.pos += step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_rate_is_passthrough() {
        let mut r = PushResampler::new(48_000, 48_000);
        let input: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        // Задержка ровно на 1 сэмпл, значения не меняются.
        assert_eq!(out.len(), 100);
        assert_eq!(out[0], 0.0);
        for i in 1..100 {
            assert_eq!(out[i], (i - 1) as f32);
        }
    }

    #[test]
    fn resample_44100_to_48000_length() {
        let mut r = PushResampler::new(44_100, 48_000);
        let input = vec![0.5f32; 44_100];
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert!((out.len() as i64 - 48_000).abs() <= 2, "len = {}", out.len());
    }

    #[test]
    fn pull_resampler_primes_and_plays() {
        let (mut p, mut c) = rtrb::RingBuffer::<f32>::new(10_000);
        let mut r = PullResampler::new(48_000, 48_000, 960);
        let mut out = vec![0.0f32; 480];
        // Пока буфер не набран — тишина.
        for _ in 0..500 {
            p.push(1.0).unwrap();
        }
        r.fill(&mut c, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        for _ in 0..1000 {
            p.push(1.0).unwrap();
        }
        r.fill(&mut c, &mut out);
        assert!(out[10..].iter().all(|&s| (s - 1.0).abs() < 1e-6));
    }
}
