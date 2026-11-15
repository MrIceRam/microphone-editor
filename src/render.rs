//! Тестовый режим без микрофона: прогоняет WAV-файл через эффект лага.
//! Удобно, чтобы послушать и подобрать настройки режимов.
//!
//!   microphone-editor --render вход.wav выход.wav --mode robot --intensity 70

use anyhow::{Context, Result, bail};

use crate::lag::{FRAME, LagEngine, LagMode, LagParams, SAMPLE_RATE};
use crate::resample::PushResampler;

pub struct RenderOptions {
    pub input: String,
    pub output: String,
    pub mode: LagMode,
    pub intensity: f32,
    pub catchup_skip: bool,
    pub catchup_speed: f32,
    pub bitrate: i32,
    /// Сколько секунд в начале оставить без лага (чтобы было с чем сравнить).
    pub clean_start: f32,
}

pub fn render(o: &RenderOptions) -> Result<()> {
    let mut reader = hound::WavReader::open(&o.input).with_context(|| format!("не открыть {}", o.input))?;
    let spec = reader.spec();
    let ch = spec.channels.max(1) as usize;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<_, _>>()?
        }
    };
    if samples.is_empty() {
        bail!("файл пустой");
    }
    let mono: Vec<f32> = samples.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect();

    let mut resampled = Vec::with_capacity(mono.len() * 2);
    PushResampler::new(spec.sample_rate, SAMPLE_RATE).process(&mono, &mut resampled);
    // Дополняем тишиной до целого числа пакетов (+ хвост на догон).
    let frames = resampled.len().div_ceil(FRAME) + 50;
    resampled.resize(frames * FRAME, 0.0);

    let mut lag = LagEngine::new()?;
    let mut out = vec![0.0f32; frames * FRAME];
    let clean_frames = (o.clean_start * SAMPLE_RATE as f32 / FRAME as f32) as usize;
    for f in 0..frames {
        let p = LagParams {
            active: f >= clean_frames && f + 40 < frames,
            mode: o.mode,
            intensity: o.intensity,
            catchup_skip: o.catchup_skip,
            catchup_speed: o.catchup_speed,
            bitrate: o.bitrate,
        };
        let r = f * FRAME..(f + 1) * FRAME;
        lag.process(&resampled[r.clone()], &mut out[r], &p);
    }

    let spec_out = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&o.output, spec_out).with_context(|| format!("не создать {}", o.output))?;
    for s in out {
        writer.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}
