//! Звуковой движок: микрофон -> эффект лага -> виртуальный кабель (+ наушники).
//!
//! Схема потоков:
//!   [поток микрофона]  читает микрофон, переводит в 48 кГц моно, режет на
//!                      пакеты по 20 мс, прогоняет через эффект лага и кладёт
//!                      результат в кольцевые буферы — по одному на каждый выход.
//!   [поток выхода]     (кабель / наушники) забирает звук из своего буфера.
//!   [поток движка]     открывает устройства и переоткрывает их при смене
//!                      настроек или если устройство отвалилось.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use crate::lag::{FRAME, LagEngine, SAMPLE_RATE};
use crate::resample::{PullResampler, PushResampler};
use crate::state::{AtomicF32, EngineStatus, Shared};

/// Настройки устройств (то, что требует переоткрыть звук).
#[derive(Clone, Debug, PartialEq)]
pub struct AudioSettings {
    pub input_device: String,
    pub output_device: String,
    pub monitor: bool,
    pub monitor_device: String,
    pub buffer_ms: u32,
}

pub enum Cmd {
    Restart(AudioSettings),
    Shutdown,
}

/// Запускает поток движка. Управлять им — через возвращённый `Sender`.
pub fn spawn(shared: Arc<Shared>, settings: AudioSettings) -> Sender<Cmd> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("audio-engine".into())
        .spawn(move || engine_thread(shared, settings, rx))
        .expect("не удалось запустить звуковой поток");
    tx
}

fn engine_thread(shared: Arc<Shared>, mut settings: AudioSettings, rx: Receiver<Cmd>) {
    let mut running: Option<Vec<cpal::Stream>> = None;
    let mut need_start = true;
    loop {
        if need_start {
            running = None; // сначала закрываем старые потоки
            shared.stream_failed.store(false, Ordering::Relaxed);
            match start(&settings, &shared) {
                Ok((streams, status)) => {
                    running = Some(streams);
                    shared.set_status(status);
                    need_start = false;
                }
                Err(e) => {
                    shared.set_status(EngineStatus {
                        running: false,
                        error: Some(format!("{e:#}")),
                        ..Default::default()
                    });
                }
            }
        }
        let wait = if need_start { Duration::from_secs(2) } else { Duration::from_millis(500) };
        match rx.recv_timeout(wait) {
            Ok(Cmd::Restart(s)) => {
                settings = s;
                need_start = true;
            }
            Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if shared.stream_failed.swap(false, Ordering::Relaxed) {
                    // Устройство отвалилось (выдернули, сменили формат в Windows...).
                    need_start = true;
                }
            }
        }
    }
    drop(running);
}

// ---------------------------------------------------------------------------
// Поиск устройств
// ---------------------------------------------------------------------------

pub fn device_name(d: &cpal::Device) -> String {
    d.description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<без имени>".into())
}

/// Списки названий (микрофоны, выходы) для окна.
pub fn list_devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let inputs = host
        .input_devices()
        .map(|it| it.map(|d| device_name(&d)).collect())
        .unwrap_or_default();
    let outputs = host
        .output_devices()
        .map(|it| it.map(|d| device_name(&d)).collect())
        .unwrap_or_default();
    (inputs, outputs)
}

/// Ищет устройство: сначала точное совпадение названия, потом по части названия.
fn find_device(devices: Vec<cpal::Device>, wanted: &str) -> Option<cpal::Device> {
    let wanted_lc = wanted.trim().to_lowercase();
    let named: Vec<(String, cpal::Device)> = devices
        .into_iter()
        .map(|d| (device_name(&d).to_lowercase(), d))
        .collect();
    if let Some(i) = named.iter().position(|(n, _)| *n == wanted_lc) {
        return named.into_iter().nth(i).map(|(_, d)| d);
    }
    named
        .into_iter()
        .find(|(n, _)| n.contains(&wanted_lc))
        .map(|(_, d)| d)
}

fn find_input(host: &cpal::Host, wanted: &str) -> Result<cpal::Device> {
    if wanted.trim().is_empty() {
        return host
            .default_input_device()
            .ok_or_else(|| anyhow!("В Windows не выбран микрофон по умолчанию"));
    }
    let devices: Vec<_> = host.input_devices()?.collect();
    find_device(devices, wanted).ok_or_else(|| anyhow!("Микрофон «{wanted}» не найден"))
}

fn find_output(host: &cpal::Host, wanted: &str, allow_default: bool) -> Result<Option<cpal::Device>> {
    if wanted.trim().is_empty() {
        return Ok(if allow_default { host.default_output_device() } else { None });
    }
    let devices: Vec<_> = host.output_devices()?.collect();
    Ok(find_device(devices, wanted))
}

// ---------------------------------------------------------------------------
// Запуск потоков
// ---------------------------------------------------------------------------

/// Ёмкость кольцевого буфера на каждый выход: 1 секунда.
const RING_CAPACITY: usize = SAMPLE_RATE as usize;

fn start(settings: &AudioSettings, shared: &Arc<Shared>) -> Result<(Vec<cpal::Stream>, EngineStatus)> {
    let host = cpal::default_host();
    let mut status = EngineStatus::default();
    let mut streams = Vec::new();
    let mut producers = Vec::new();
    let target_fill = (settings.buffer_ms.clamp(10, 300) as usize * SAMPLE_RATE as usize / 1000).max(FRAME / 2);

    // --- Выход в виртуальный кабель.
    match find_output(&host, &settings.output_device, false) {
        Ok(Some(dev)) => {
            let (prod, cons) = rtrb::RingBuffer::new(RING_CAPACITY);
            match build_output(&dev, cons, target_fill, shared.clone(), OutputKind::Main) {
                Ok(s) => {
                    streams.push(s);
                    producers.push(prod);
                    status.output_name = Some(device_name(&dev));
                }
                Err(e) => status.warnings.push(format!("Не удалось открыть выход «{}»: {e:#}", device_name(&dev))),
            }
        }
        Ok(None) if settings.output_device.trim().is_empty() => {
            status.warnings.push("Не выбран выход. Выбери «CABLE Input (VB-Audio Virtual Cable)».".into())
        }
        Ok(None) => status.warnings.push(format!(
            "Не найден выход «{}». Установи VB-Audio Virtual Cable (vb-audio.com) и перезагрузи компьютер.",
            settings.output_device
        )),
        Err(e) => status.warnings.push(format!("Ошибка поиска выхода: {e:#}")),
    }

    // --- Прослушка в наушники.
    if settings.monitor {
        match find_output(&host, &settings.monitor_device, true) {
            Ok(Some(dev)) => {
                let (prod, cons) = rtrb::RingBuffer::new(RING_CAPACITY);
                match build_output(&dev, cons, target_fill, shared.clone(), OutputKind::Monitor) {
                    Ok(s) => {
                        streams.push(s);
                        producers.push(prod);
                        status.monitor_name = Some(device_name(&dev));
                    }
                    Err(e) => status.warnings.push(format!("Не удалось открыть наушники: {e:#}")),
                }
            }
            Ok(None) => status.warnings.push(format!("Наушники «{}» не найдены", settings.monitor_device)),
            Err(e) => status.warnings.push(format!("Ошибка поиска наушников: {e:#}")),
        }
    }

    // --- Микрофон (без него работать нечему — это уже ошибка).
    let input = find_input(&host, &settings.input_device)?;
    let in_cfg = input
        .default_input_config()
        .with_context(|| format!("не удалось узнать формат микрофона «{}»", device_name(&input)))?;
    let processor = Processor::new(in_cfg.sample_rate(), producers, shared.clone())?;
    let in_stream = build_input(&input, &in_cfg, processor, shared.clone())
        .with_context(|| format!("не удалось открыть микрофон «{}»", device_name(&input)))?;
    status.input_name = device_name(&input);
    streams.push(in_stream);

    for s in &streams {
        s.play().context("не удалось запустить звук")?;
    }
    status.running = true;
    Ok((streams, status))
}

/// Обработка звука микрофона (живёт внутри потока микрофона).
struct Processor {
    resampler: PushResampler,
    mono: Vec<f32>,
    pending: Vec<f32>,
    frame_in: [f32; FRAME],
    frame_out: [f32; FRAME],
    lag: LagEngine,
    outputs: Vec<rtrb::Producer<f32>>,
    shared: Arc<Shared>,
}

impl Processor {
    fn new(in_rate: u32, outputs: Vec<rtrb::Producer<f32>>, shared: Arc<Shared>) -> Result<Self> {
        Ok(Self {
            resampler: PushResampler::new(in_rate, SAMPLE_RATE),
            mono: Vec::with_capacity(8192),
            pending: Vec::with_capacity(FRAME * 8),
            frame_in: [0.0; FRAME],
            frame_out: [0.0; FRAME],
            lag: LagEngine::new()?,
            outputs,
            shared,
        })
    }

    /// `self.mono` уже заполнен сэмплами микрофона (моно, родная частота).
    fn process(&mut self) {
        self.resampler.process(&self.mono, &mut self.pending);
        let mut consumed = 0;
        while self.pending.len() - consumed >= FRAME {
            let gain = self.shared.input_gain.get();
            let mut in_peak = 0.0f32;
            for (dst, &src) in self.frame_in.iter_mut().zip(&self.pending[consumed..consumed + FRAME]) {
                *dst = src * gain;
                in_peak = in_peak.max(dst.abs());
            }
            consumed += FRAME;

            let params = self.shared.lag_params();
            self.lag.process(&self.frame_in, &mut self.frame_out, &params);

            let mut out_peak = 0.0f32;
            for s in self.frame_out.iter_mut() {
                *s = s.clamp(-1.0, 1.0);
                out_peak = out_peak.max(s.abs());
            }
            for prod in self.outputs.iter_mut() {
                if prod.slots() >= FRAME {
                    for &s in self.frame_out.iter() {
                        let _ = prod.push(s);
                    }
                }
            }
            update_level(&self.shared.in_level, in_peak);
            update_level(&self.shared.out_level, out_peak);
            self.shared.lag_wet.set(self.lag.wet_gain());
        }
        self.pending.drain(..consumed);
    }
}

/// Индикатор уровня: мгновенный подъём, плавное падение.
fn update_level(level: &AtomicF32, peak: f32) {
    let old = level.get();
    level.set(peak.max(old * 0.85));
}

fn build_input(
    dev: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    processor: Processor,
    shared: Arc<Shared>,
) -> Result<cpal::Stream> {
    let config = cfg.config();
    match cfg.sample_format() {
        SampleFormat::F32 => build_input_typed::<f32>(dev, config, processor, shared),
        SampleFormat::I16 => build_input_typed::<i16>(dev, config, processor, shared),
        SampleFormat::I32 => build_input_typed::<i32>(dev, config, processor, shared),
        SampleFormat::U16 => build_input_typed::<u16>(dev, config, processor, shared),
        SampleFormat::F64 => build_input_typed::<f64>(dev, config, processor, shared),
        other => bail!("неподдерживаемый формат микрофона: {other:?}"),
    }
}

fn build_input_typed<T>(
    dev: &cpal::Device,
    config: cpal::StreamConfig,
    mut processor: Processor,
    shared: Arc<Shared>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels.max(1) as usize;
    let stream = dev.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            processor.mono.clear();
            for frame in data.chunks_exact(channels) {
                let sum: f32 = frame.iter().map(|s| s.to_sample::<f32>()).sum();
                processor.mono.push(sum / channels as f32);
            }
            processor.process();
        },
        move |err| {
            eprintln!("Ошибка микрофона: {err}");
            shared.stream_failed.store(true, Ordering::Relaxed);
        },
        None,
    )?;
    Ok(stream)
}

#[derive(Clone, Copy, PartialEq)]
enum OutputKind {
    /// Виртуальный кабель (то, что слышат в Discord).
    Main,
    /// Наушники (громкость регулируется отдельно).
    Monitor,
}

fn build_output(
    dev: &cpal::Device,
    cons: rtrb::Consumer<f32>,
    target_fill: usize,
    shared: Arc<Shared>,
    kind: OutputKind,
) -> Result<cpal::Stream> {
    let cfg = dev.default_output_config()?;
    let config = cfg.config();
    match cfg.sample_format() {
        SampleFormat::F32 => build_output_typed::<f32>(dev, config, cons, target_fill, shared, kind),
        SampleFormat::I16 => build_output_typed::<i16>(dev, config, cons, target_fill, shared, kind),
        SampleFormat::I32 => build_output_typed::<i32>(dev, config, cons, target_fill, shared, kind),
        SampleFormat::U16 => build_output_typed::<u16>(dev, config, cons, target_fill, shared, kind),
        SampleFormat::F64 => build_output_typed::<f64>(dev, config, cons, target_fill, shared, kind),
        other => bail!("неподдерживаемый формат вывода: {other:?}"),
    }
}

fn build_output_typed<T>(
    dev: &cpal::Device,
    config: cpal::StreamConfig,
    mut cons: rtrb::Consumer<f32>,
    target_fill: usize,
    shared: Arc<Shared>,
    kind: OutputKind,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels.max(1) as usize;
    let mut resampler = PullResampler::new(SAMPLE_RATE, config.sample_rate, target_fill);
    let mut mono: Vec<f32> = Vec::with_capacity(8192);
    let data_shared = shared.clone();
    let stream = dev.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _| {
            let frames = data.len() / channels;
            mono.resize(frames, 0.0);
            resampler.fill(&mut cons, &mut mono);
            let volume = match kind {
                OutputKind::Main => 1.0,
                OutputKind::Monitor => data_shared.monitor_volume.get(),
            };
            for (frame, &s) in data.chunks_exact_mut(channels).zip(mono.iter()) {
                let v = T::from_sample(s * volume);
                frame.fill(v);
            }
        },
        move |err| {
            eprintln!("Ошибка вывода звука: {err}");
            shared.stream_failed.store(true, Ordering::Relaxed);
        },
        None,
    )?;
    Ok(stream)
}
