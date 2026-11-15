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
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use crate::lag::{FRAME, LagEngine, SAMPLE_RATE};
use crate::piano::{self, PianoSynth};
use crate::resample::{PullResampler, PushResampler};
use crate::state::{AtomicF32, EngineStatus, Shared, StreamStats};
use crate::virtual_mic;

/// Настройки устройств (то, что требует переоткрыть звук).
#[derive(Clone, Debug, PartialEq)]
pub struct AudioSettings {
    pub input_device: String,
    pub output_device: String,
    /// Слышать свой голос в наушниках.
    pub monitor: bool,
    /// Слышать пианино в наушниках.
    pub monitor_piano: bool,
    pub monitor_device: String,
    pub buffer_ms: u32,
    /// Включён ли звук пианино.
    pub piano: bool,
    /// Файл SoundFont (имя в папке soundfonts, полный путь или пусто).
    pub soundfont: String,
}

/// Загруженный SoundFont (чтобы не читать 30 МБ заново при каждой смене устройства).
type SoundFontCache = Option<(std::path::PathBuf, Arc<rustysynth::SoundFont>)>;

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
    let mut sf_cache: SoundFontCache = None;
    loop {
        if need_start {
            running = None; // сначала закрываем старые потоки
            shared.stream_failed.store(false, Ordering::Relaxed);
            match start(&settings, &shared, &mut sf_cache) {
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
                    // Небольшая пауза, чтобы Windows успела «отпустить» устройство.
                    thread::sleep(Duration::from_secs(1));
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
pub(crate) fn find_device(devices: Vec<cpal::Device>, wanted: &str) -> Option<cpal::Device> {
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

/// Ищет микрофон. `cable_driver` — драйвер виртуального кабеля, в который пишет
/// программа: его микрофон брать нельзя, иначе программа будет слушать сама себя.
/// Второе значение — предупреждение для окна.
fn find_input(host: &cpal::Host, wanted: &str, cable_driver: Option<&str>) -> Result<(cpal::Device, Option<String>)> {
    let is_loop = |d: &cpal::Device| {
        let name = device_name(d);
        cable_driver.is_some() && virtual_mic::driver_part(&name) == cable_driver
    };
    if wanted.trim().is_empty() {
        let default = host
            .default_input_device()
            .ok_or_else(|| anyhow!("В Windows не выбран микрофон по умолчанию"))?;
        if !is_loop(&default) {
            return Ok((default, None));
        }
        // Микрофон по умолчанию в Windows — наш же виртуальный. Берём настоящий.
        let real = host
            .input_devices()?
            .find(|d| !is_loop(d) && !virtual_mic::driver_part(&device_name(d)).is_some_and(virtual_mic::is_virtual_driver))
            .ok_or_else(|| anyhow!("Не нашёл настоящий микрофон: в Windows по умолчанию стоит виртуальный"))?;
        let warn = format!(
            "В Windows по умолчанию стоит виртуальный микрофон, поэтому программа слушает «{}». Выбери свой микрофон на вкладке «Устройства».",
            device_name(&real)
        );
        return Ok((real, Some(warn)));
    }
    let devices: Vec<_> = host.input_devices()?.collect();
    let dev = find_device(devices, wanted).ok_or_else(|| anyhow!("Микрофон «{wanted}» не найден"))?;
    if is_loop(&dev) {
        bail!("Микрофон программы — это её же виртуальный микрофон «{}», получилось бы эхо. Выбери настоящий микрофон", device_name(&dev));
    }
    Ok((dev, None))
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

fn start(
    settings: &AudioSettings,
    shared: &Arc<Shared>,
    sf_cache: &mut SoundFontCache,
) -> Result<(Vec<cpal::Stream>, EngineStatus)> {
    let host = cpal::default_host();
    let mut status = EngineStatus::default();
    let mut streams = Vec::new();
    let mut producers = Vec::new();
    let target_fill = (settings.buffer_ms.clamp(10, 300) as usize * SAMPLE_RATE as usize / 1000).max(FRAME / 2);

    // --- Синтезатор пианино.
    let synth = if settings.piano {
        match load_synth(&settings.soundfont, sf_cache) {
            Ok((s, name)) => {
                status.soundfont_name = Some(name);
                Some(s)
            }
            Err(e) => {
                status.warnings.push(format!("Пианино без звука: {e:#}"));
                None
            }
        }
    } else {
        None
    };

    // --- Выход в виртуальный кабель (пусто = найти автоматически).
    let (in_names, out_names) = list_devices();
    let mics = virtual_mic::pair_virtual_mics(&in_names, &out_names);
    let output_wanted = if settings.output_device.trim().is_empty() {
        mics.first().map(|m| m.render.clone()).unwrap_or_default()
    } else {
        settings.output_device.clone()
    };
    let pair = virtual_mic::pair_for_output(&mics, &output_wanted).cloned();
    match find_output(&host, &output_wanted, false) {
        Ok(Some(dev)) => {
            let (prod, cons) = rtrb::RingBuffer::new(RING_CAPACITY);
            match build_output(&dev, cons, target_fill, shared.clone(), OutputKind::Main) {
                Ok(s) => {
                    streams.push(s);
                    producers.push((OutputKind::Main, prod));
                    status.output_name = Some(device_name(&dev));
                    status.virtual_capture = pair.as_ref().map(|p| p.capture.clone());
                }
                Err(e) => status.warnings.push(format!("Не удалось открыть выход «{}»: {e:#}", device_name(&dev))),
            }
        }
        Ok(None) if settings.output_device.trim().is_empty() => status.warnings.push(
            "Не найден выход: в системе нет виртуального микрофона (VB-Audio Virtual Cable или Steam Streaming Microphone).".into(),
        ),
        Ok(None) => status.warnings.push(format!(
            "Не найден выход «{}». Установи VB-Audio Virtual Cable (vb-audio.com) и перезагрузи компьютер.",
            settings.output_device
        )),
        Err(e) => status.warnings.push(format!("Ошибка поиска выхода: {e:#}")),
    }

    // --- Прослушка в наушники (свой голос и/или пианино).
    if settings.monitor || (settings.monitor_piano && synth.is_some()) {
        match find_output(&host, &settings.monitor_device, true) {
            // Наушники = основной выход: там и так всё слышно, второй поток дал бы эхо.
            Ok(Some(dev)) if status.output_name.as_deref() == Some(device_name(&dev).as_str()) => {}
            Ok(Some(dev)) => {
                let (prod, cons) = rtrb::RingBuffer::new(RING_CAPACITY);
                match build_output(&dev, cons, target_fill, shared.clone(), OutputKind::Monitor) {
                    Ok(s) => {
                        streams.push(s);
                        producers.push((OutputKind::Monitor, prod));
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
    let cable_driver = status.output_name.as_deref().and_then(virtual_mic::driver_part).map(str::to_string);
    let cable_driver = cable_driver.filter(|d| virtual_mic::is_virtual_driver(d));
    let (input, input_warning) = find_input(&host, &settings.input_device, cable_driver.as_deref())?;
    status.warnings.extend(input_warning);
    let in_cfg = input
        .default_input_config()
        .with_context(|| format!("не удалось узнать формат микрофона «{}»", device_name(&input)))?;
    shared.input_stats.sample_rate.store(in_cfg.sample_rate(), Ordering::Relaxed);
    shared.input_stats.channels.store(in_cfg.channels() as u32, Ordering::Relaxed);
    // Очередь нот: MIDI-поток и окно кладут, звуковой поток забирает.
    let (piano_tx, piano_rx) = rtrb::RingBuffer::new(1024);
    *shared.piano_tx.lock().unwrap() = Some(piano_tx);
    let processor = Processor::new(in_cfg.sample_rate(), producers, synth, piano_rx, shared.clone())?;
    let in_stream = build_input(&input, &in_cfg, processor, shared.clone())
        .with_context(|| format!("не удалось открыть микрофон «{}»", device_name(&input)))?;
    status.input_name = device_name(&input);
    streams.push(in_stream);

    shared.stream_start_ms.store(crate::state::now_ms(), Ordering::Relaxed);
    for s in &streams {
        s.play().context("не удалось запустить звук")?;
    }
    status.running = true;
    Ok((streams, status))
}

/// Загружает SoundFont (или берёт из кэша) и создаёт синтезатор.
fn load_synth(setting: &str, cache: &mut SoundFontCache) -> Result<(PianoSynth, String)> {
    let path = piano::resolve_soundfont(setting).ok_or_else(|| {
        if setting.trim().is_empty() {
            anyhow!("нет файлов .sf2 в папке soundfonts")
        } else {
            anyhow!("SoundFont «{setting}» не найден в папке soundfonts")
        }
    })?;
    let sf = match cache {
        Some((p, sf)) if *p == path => sf.clone(),
        _ => {
            let sf = piano::load_soundfont(&path)?;
            *cache = Some((path.clone(), sf.clone()));
            sf
        }
    };
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    Ok((PianoSynth::new(&sf)?, name))
}

/// Обработка звука микрофона (живёт внутри потока микрофона).
struct Processor {
    resampler: PushResampler,
    mono: Vec<f32>,
    pending: Vec<f32>,
    frame_in: [f32; FRAME],
    frame_out: [f32; FRAME],
    /// Звук пианино за текущий пакет.
    frame_piano: [f32; FRAME],
    /// Что идёт в наушники.
    frame_monitor: [f32; FRAME],
    lag: LagEngine,
    synth: Option<PianoSynth>,
    piano_rx: rtrb::Consumer<[u8; 3]>,
    piano_was_enabled: bool,
    outputs: Vec<(OutputKind, rtrb::Producer<f32>)>,
    shared: Arc<Shared>,
}

impl Processor {
    fn new(
        in_rate: u32,
        outputs: Vec<(OutputKind, rtrb::Producer<f32>)>,
        synth: Option<PianoSynth>,
        piano_rx: rtrb::Consumer<[u8; 3]>,
        shared: Arc<Shared>,
    ) -> Result<Self> {
        Ok(Self {
            resampler: PushResampler::new(in_rate, SAMPLE_RATE),
            mono: Vec::with_capacity(8192),
            pending: Vec::with_capacity(FRAME * 8),
            frame_in: [0.0; FRAME],
            frame_out: [0.0; FRAME],
            frame_piano: [0.0; FRAME],
            frame_monitor: [0.0; FRAME],
            lag: LagEngine::new()?,
            synth,
            piano_rx,
            piano_was_enabled: true,
            outputs,
            shared,
        })
    }

    /// Забирает ноты из очереди и рендерит пакет пианино в `frame_piano`.
    fn render_piano(&mut self) {
        let enabled = self.shared.piano_enabled.load(Ordering::Relaxed);
        let transpose = self.shared.piano_transpose.load(Ordering::Relaxed);
        let Some(synth) = self.synth.as_mut() else {
            while self.piano_rx.pop().is_ok() {}
            self.frame_piano.fill(0.0);
            return;
        };
        while let Ok(msg) = self.piano_rx.pop() {
            if enabled {
                synth.handle(msg, transpose);
            }
        }
        if !enabled {
            if self.piano_was_enabled {
                synth.all_notes_off();
            }
            self.piano_was_enabled = false;
            self.frame_piano.fill(0.0);
            return;
        }
        self.piano_was_enabled = true;
        synth.set_program(self.shared.piano_program.load(Ordering::Relaxed));
        synth.render(&mut self.frame_piano);
        let volume = self.shared.piano_volume.get();
        for s in self.frame_piano.iter_mut() {
            *s *= volume;
        }
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

            // Пианино: либо до эффекта лага (лагает вместе с голосом), либо после.
            self.render_piano();
            let through_lag = self.shared.piano_through_lag.load(Ordering::Relaxed);
            if through_lag {
                for (d, p) in self.frame_in.iter_mut().zip(&self.frame_piano) {
                    *d += p;
                }
            }

            let params = self.shared.lag_params();
            self.lag.process(&self.frame_in, &mut self.frame_out, &params);

            if !through_lag {
                for (d, p) in self.frame_out.iter_mut().zip(&self.frame_piano) {
                    *d += p;
                }
            }

            let mut out_peak = 0.0f32;
            for s in self.frame_out.iter_mut() {
                *s = s.clamp(-1.0, 1.0);
                out_peak = out_peak.max(s.abs());
            }

            // В наушники: весь микс (как слышит Discord) или только пианино.
            let monitor_voice = self.shared.monitor_voice.load(Ordering::Relaxed);
            let monitor_piano = self.shared.monitor_piano.load(Ordering::Relaxed);
            for i in 0..FRAME {
                self.frame_monitor[i] = if monitor_voice {
                    self.frame_out[i]
                } else if monitor_piano {
                    self.frame_piano[i].clamp(-1.0, 1.0)
                } else {
                    0.0
                };
            }

            for (kind, prod) in self.outputs.iter_mut() {
                let frame = match kind {
                    OutputKind::Main => &self.frame_out,
                    OutputKind::Monitor => &self.frame_monitor,
                };
                if prod.slots() >= FRAME {
                    for &s in frame.iter() {
                        let _ = prod.push(s);
                    }
                } else {
                    stats_for(&self.shared, *kind).dropped.fetch_add(1, Ordering::Relaxed);
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
    let mut last_call: Option<Instant> = None;
    let stream = dev.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            let started = Instant::now();
            let gap_us = last_call.map(|t| (started - t).as_micros() as u64).unwrap_or(0);
            last_call = Some(started);

            processor.mono.clear();
            for frame in data.chunks_exact(channels) {
                let sum: f32 = frame.iter().map(|s| s.to_sample::<f32>()).sum();
                processor.mono.push(sum / channels as f32);
            }
            processor.process();

            let work_us = started.elapsed().as_micros() as u64;
            processor
                .shared
                .input_stats
                .on_callback(data.len() / channels, gap_us, work_us);
        },
        move |err| handle_stream_error(&shared, &shared.input_stats, "микрофон", err),
        None,
    )?;
    Ok(stream)
}

/// Ошибки звуковых потоков. Сбой буфера (Xrun) — не поломка: Windows просто
/// сообщает, что потерялся кусочек звука. Его только считаем. Перезапускаем
/// звук лишь когда устройство действительно пропало или сменилось.
fn handle_stream_error(shared: &Shared, stats: &StreamStats, what: &str, err: cpal::Error) {
    use cpal::ErrorKind;
    match err.kind() {
        ErrorKind::Xrun => {
            stats.xruns.fetch_add(1, Ordering::Relaxed);
            if !shared.in_warmup() {
                stats.underruns_total.fetch_add(1, Ordering::Relaxed);
            }
        }
        ErrorKind::DeviceNotAvailable
        | ErrorKind::StreamInvalidated
        | ErrorKind::DeviceChanged
        | ErrorKind::BackendError => {
            eprintln!("Ошибка ({what}): {err} — переподключаюсь");
            shared.stream_failed.store(true, Ordering::Relaxed);
        }
        _ => eprintln!("Предупреждение ({what}): {err}"),
    }
}

fn stats_for(shared: &Shared, kind: OutputKind) -> &StreamStats {
    match kind {
        OutputKind::Main => &shared.main_stats,
        OutputKind::Monitor => &shared.monitor_stats,
    }
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
    let stats = stats_for(&shared, kind);
    stats.sample_rate.store(config.sample_rate, Ordering::Relaxed);
    stats.channels.store(channels as u32, Ordering::Relaxed);
    let mut resampler = PullResampler::new(SAMPLE_RATE, config.sample_rate, target_fill);
    let mut mono: Vec<f32> = Vec::with_capacity(8192);
    let mut last_call: Option<Instant> = None;
    let data_shared = shared.clone();
    let stream = dev.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _| {
            let started = Instant::now();
            let gap_us = last_call.map(|t| (started - t).as_micros() as u64).unwrap_or(0);
            last_call = Some(started);

            let frames = data.len() / channels;
            mono.resize(frames, 0.0);
            let report = resampler.fill(&mut cons, &mut mono);
            let volume = if data_shared.mute_outputs.load(Ordering::Relaxed) {
                0.0
            } else {
                match kind {
                    OutputKind::Main => data_shared.main_gain.get(),
                    OutputKind::Monitor => data_shared.monitor_volume.get(),
                }
            };
            for (frame, &s) in data.chunks_exact_mut(channels).zip(mono.iter()) {
                let v = T::from_sample(s * volume);
                frame.fill(v);
            }

            let stats = stats_for(&data_shared, kind);
            stats.on_callback(frames, gap_us, started.elapsed().as_micros() as u64);
            stats.min_fill.fetch_min(report.fill_before as u64, Ordering::Relaxed);
            if report.underrun {
                stats.underruns.fetch_add(1, Ordering::Relaxed);
                if !data_shared.in_warmup() {
                    stats.underruns_total.fetch_add(1, Ordering::Relaxed);
                }
            }
            if report.silent {
                stats.silent_callbacks.fetch_add(1, Ordering::Relaxed);
            }
        },
        move |err| handle_stream_error(&shared, stats_for(&shared, kind), "выход", err),
        None,
    )?;
    Ok(stream)
}
