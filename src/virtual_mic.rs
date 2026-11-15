//! Виртуальный микрофон.
//!
//! Обычная программа не может сама создать микрофон в Windows — это умеет
//! только звуковой драйвер. Поэтому используется «виртуальный кабель»:
//! драйвер с парой устройств. Что играется в его выход (динамики), то
//! появляется в его микрофоне. Программа пишет обработанный голос в выход,
//! а Discord и игры слушают микрофон кабеля.
//!
//! Подходящие кабели: VB-Audio Virtual Cable и Steam Streaming Microphone
//! (ставится вместе со Steam). Микрофон кабеля можно переименовать в Windows,
//! например в «Micro Edition».

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use crate::engine::{device_name, find_device};
use crate::state::AtomicF32;

/// Пара устройств виртуального кабеля.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualMic {
    /// Выход, куда программа пишет звук ("CABLE Input (VB-Audio Virtual Cable)").
    pub render: String,
    /// Микрофон, который выбирают в Discord ("CABLE Output (VB-Audio Virtual Cable)").
    pub capture: String,
    /// Название драйвера ("VB-Audio Virtual Cable").
    pub driver: String,
}

/// Текст в последних скобках: "Микрофон (Steam Streaming Microphone)" -> "Steam Streaming Microphone".
pub fn driver_part(name: &str) -> Option<&str> {
    let name = name.trim_end();
    if !name.ends_with(')') {
        return None;
    }
    let open = name.rfind('(')?;
    Some(name[open + 1..name.len() - 1].trim())
}

/// Название без части в скобках: "Micro Edition (Steam Streaming Microphone)" -> "Micro Edition".
pub fn name_part(name: &str) -> &str {
    match name.rfind(" (") {
        Some(i) if name.ends_with(')') => name[..i].trim(),
        _ => name.trim(),
    }
}

/// Похоже ли название драйвера на виртуальный кабель.
pub fn is_virtual_driver(driver: &str) -> bool {
    let d = driver.to_lowercase();
    d.contains("vb-audio") || d.contains("virtual") || d.contains("steam streaming microphone") || d.contains("voicemeeter")
}

/// Насколько кабель предпочтительнее (меньше — лучше).
fn priority(driver: &str) -> u8 {
    let d = driver.to_lowercase();
    if d.contains("vb-audio virtual cable") {
        0
    } else if d.contains("vb-audio") || d.contains("virtual") {
        1
    } else if d.contains("voicemeeter") {
        2
    } else {
        3 // Steam Streaming Microphone
    }
}

/// Ищет пары «выход + микрофон» виртуальных кабелей по названиям устройств.
pub fn pair_virtual_mics(inputs: &[String], outputs: &[String]) -> Vec<VirtualMic> {
    let mut found = Vec::new();
    for out in outputs {
        let Some(driver) = driver_part(out) else { continue };
        if !is_virtual_driver(driver) {
            continue;
        }
        if let Some(cap) = inputs.iter().find(|i| driver_part(i) == Some(driver)) {
            found.push(VirtualMic {
                render: out.clone(),
                capture: cap.clone(),
                driver: driver.to_string(),
            });
        }
    }
    found.sort_by_key(|v| priority(&v.driver));
    found
}

/// Пара для данного выхода (если выход — виртуальный кабель).
pub fn pair_for_output<'a>(mics: &'a [VirtualMic], output: &str) -> Option<&'a VirtualMic> {
    let o = output.trim().to_lowercase();
    if o.is_empty() {
        return None;
    }
    mics.iter()
        .find(|m| m.render.to_lowercase() == o)
        .or_else(|| mics.iter().find(|m| m.render.to_lowercase().contains(&o)))
}

/// Громкость тестового тона.
/// Тихий — если в этот момент Discord слушает кабель, будет еле слышный писк.
const TEST_AMPLITUDE: f32 = 0.005;

/// Результат проверки кабеля.
#[derive(Debug, Clone, Copy)]
pub struct LoopbackResult {
    /// Пик, дошедший до микрофона кабеля.
    pub peak: f32,
    /// Во сколько раз кабель усиливает звук, в дБ (0 = без изменений).
    pub gain_db: f32,
}

impl LoopbackResult {
    pub fn works(&self) -> bool {
        self.peak > TEST_AMPLITUDE * 0.1
    }
}

/// Проверка кабеля: играет тихий тон в выход и слушает микрофон кабеля.
pub fn loopback_test(render: &str, capture: &str) -> Result<LoopbackResult> {
    let host = cpal::default_host();
    let out_dev = find_device(host.output_devices()?.collect(), render).ok_or_else(|| anyhow!("нет выхода «{render}»"))?;
    let in_dev = find_device(host.input_devices()?.collect(), capture).ok_or_else(|| anyhow!("нет микрофона «{capture}»"))?;

    let peak = Arc::new(AtomicF32::new(0.0));
    let in_stream = build_peak_input(&in_dev, peak.clone())?;
    let out_stream = build_tone_output(&out_dev, TEST_AMPLITUDE)?;
    in_stream.play()?;
    out_stream.play()?;
    std::thread::sleep(Duration::from_millis(400));
    peak.set(0.0);
    std::thread::sleep(Duration::from_millis(1200));
    let p = peak.get();
    drop(out_stream);
    drop(in_stream);
    Ok(LoopbackResult {
        peak: p,
        gain_db: 20.0 * (p.max(1e-6) / TEST_AMPLITUDE).log10(),
    })
}

fn build_peak_input(dev: &cpal::Device, peak: Arc<AtomicF32>) -> Result<cpal::Stream> {
    let cfg = dev.default_input_config()?;
    let config = cfg.config();
    match cfg.sample_format() {
        SampleFormat::F32 => peak_input::<f32>(dev, config, peak),
        SampleFormat::I16 => peak_input::<i16>(dev, config, peak),
        SampleFormat::I32 => peak_input::<i32>(dev, config, peak),
        SampleFormat::U16 => peak_input::<u16>(dev, config, peak),
        other => bail!("неподдерживаемый формат {other:?} у {}", device_name(dev)),
    }
}

fn peak_input<T>(dev: &cpal::Device, config: cpal::StreamConfig, peak: Arc<AtomicF32>) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    Ok(dev.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            let p = data.iter().fold(0.0f32, |m, s| m.max(s.to_sample::<f32>().abs()));
            if p > peak.get() {
                peak.set(p);
            }
        },
        |_| {},
        None,
    )?)
}

fn build_tone_output(dev: &cpal::Device, amp: f32) -> Result<cpal::Stream> {
    let cfg = dev.default_output_config()?;
    let config = cfg.config();
    match cfg.sample_format() {
        SampleFormat::F32 => tone_output::<f32>(dev, config, amp),
        SampleFormat::I16 => tone_output::<i16>(dev, config, amp),
        SampleFormat::I32 => tone_output::<i32>(dev, config, amp),
        SampleFormat::U16 => tone_output::<u16>(dev, config, amp),
        other => bail!("неподдерживаемый формат {other:?} у {}", device_name(dev)),
    }
}

fn tone_output<T>(dev: &cpal::Device, config: cpal::StreamConfig, amp: f32) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels.max(1) as usize;
    let step = 440.0 / config.sample_rate as f32;
    let mut phase = 0.0f32;
    Ok(dev.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _| {
            for frame in data.chunks_exact_mut(channels) {
                let s = (phase * std::f32::consts::TAU).sin() * amp;
                phase = (phase + step).fract();
                frame.fill(T::from_sample(s));
            }
        },
        |_| {},
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn pairs_cables_and_skips_real_headsets() {
        let inputs = s(&[
            "Микрофон (DP-HS-1015)",
            "Микрофон (Steam Streaming Microphone)",
            "CABLE Output (VB-Audio Virtual Cable)",
        ]);
        let outputs = s(&[
            "Динамики (DP-HS-1015)",
            "Динамики (Steam Streaming Microphone)",
            "Динамики (Steam Streaming Speakers)",
            "CABLE Input (VB-Audio Virtual Cable)",
        ]);
        let mics = pair_virtual_mics(&inputs, &outputs);
        assert_eq!(mics.len(), 2, "{mics:?}");
        assert_eq!(mics[0].render, "CABLE Input (VB-Audio Virtual Cable)");
        assert_eq!(mics[0].capture, "CABLE Output (VB-Audio Virtual Cable)");
        assert_eq!(mics[1].capture, "Микрофон (Steam Streaming Microphone)");
        assert_eq!(pair_for_output(&mics, "CABLE Input").map(|m| m.driver.as_str()), Some("VB-Audio Virtual Cable"));
        assert!(pair_for_output(&mics, "Динамики (DP-HS-1015)").is_none());
    }

    #[test]
    fn renamed_microphone_still_pairs() {
        let inputs = s(&["Micro Edition (Steam Streaming Microphone)"]);
        let outputs = s(&["Динамики (Steam Streaming Microphone)"]);
        let mics = pair_virtual_mics(&inputs, &outputs);
        assert_eq!(mics.len(), 1);
        assert_eq!(name_part(&mics[0].capture), "Micro Edition");
    }
}
