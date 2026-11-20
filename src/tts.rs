//! Озвучка текста голосами Windows (Ирина, Павел — работают без интернета).
//!
//! Отдельный поток берёт сообщения из очереди, синтезирует речь и понемногу
//! кладёт звук в кольцевой буфер. Звуковой движок подмешивает его в
//! виртуальный микрофон и в наушники. Так «Пропустить» срабатывает сразу,
//! а не после конца длинного сообщения.

use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use windows::Media::SpeechSynthesis::SpeechSynthesizer;
use windows::Storage::Streams::DataReader;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::HSTRING;

use crate::state::LockExt;
use crate::lag::SAMPLE_RATE;
use crate::resample::PushResampler;
use crate::state::Shared;

/// Ёмкость буфера озвучки: 2 секунды (чтобы «Пропустить» срабатывал быстро).
pub const TTS_RING: usize = SAMPLE_RATE as usize * 2;
/// Больше сообщений в очереди не копим — лишние выкидываются.
const MAX_QUEUE: usize = 20;

/// Сообщение на озвучку.
pub struct Job {
    /// Что произнести.
    pub text: String,
    /// Как показать в окне («Ник: текст»).
    pub label: String,
}

/// Названия установленных голосов («Microsoft Irina»).
pub fn voice_names() -> Vec<String> {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let Ok(voices) = SpeechSynthesizer::AllVoices() else {
        return Vec::new();
    };
    let n = voices.Size().unwrap_or(0);
    (0..n)
        .filter_map(|i| voices.GetAt(i).ok())
        .filter_map(|v| v.DisplayName().ok().map(|s| s.to_string()))
        .collect()
}

struct Synth {
    synth: SpeechSynthesizer,
    voice: String,
}

impl Synth {
    fn new() -> Result<Self> {
        Ok(Self {
            synth: SpeechSynthesizer::new().context("голоса Windows недоступны")?,
            voice: String::new(),
        })
    }

    /// Выбирает голос по части названия; пусто = первый русский.
    fn set_voice(&mut self, wanted: &str) {
        if self.voice == wanted {
            return;
        }
        self.voice = wanted.to_string();
        let Ok(voices) = SpeechSynthesizer::AllVoices() else { return };
        let w = wanted.trim().to_lowercase();
        let n = voices.Size().unwrap_or(0);
        let all: Vec<_> = (0..n).filter_map(|i| voices.GetAt(i).ok()).collect();
        let pick = all
            .iter()
            .find(|v| !w.is_empty() && v.DisplayName().map(|s| s.to_string().to_lowercase().contains(&w)).unwrap_or(false))
            .or_else(|| {
                all.iter()
                    .find(|v| v.Language().map(|l| l.to_string().starts_with("ru")).unwrap_or(false))
            })
            .or(all.first());
        if let Some(v) = pick {
            let _ = self.synth.SetVoice(v);
        }
    }

    /// Текст -> звук 48 кГц моно.
    fn speak(&self, text: &str) -> Result<Vec<f32>> {
        let stream = self
            .synth
            .SynthesizeTextToStreamAsync(&HSTRING::from(text))?
            .join()?;
        let size = stream.Size()? as u32;
        let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
        reader.LoadAsync(size)?.join()?;
        let mut bytes = vec![0u8; size as usize];
        reader.ReadBytes(&mut bytes)?;
        decode_wav(&bytes)
    }
}

/// WAV из синтезатора -> 48 кГц моно f32.
fn decode_wav(bytes: &[u8]) -> Result<Vec<f32>> {
    let mut wav = hound::WavReader::new(Cursor::new(bytes)).map_err(|e| anyhow!("синтезатор вернул не WAV: {e}"))?;
    let spec = wav.spec();
    let ch = spec.channels.max(1) as usize;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => wav.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            wav.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>()?
        }
    };
    let mono: Vec<f32> = samples.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect();
    let mut out = Vec::with_capacity(mono.len() * 3);
    PushResampler::new(spec.sample_rate, SAMPLE_RATE).process(&mono, &mut out);
    Ok(out)
}

/// Озвучивает текст сразу в звук (для проверки из командной строки).
pub fn synthesize(text: &str, voice: &str) -> Result<Vec<f32>> {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let mut s = Synth::new()?;
    s.set_voice(voice);
    s.speak(text)
}

/// Готовит сообщение зрителя к озвучке: убирает ссылки, обрезает длину.
pub fn prepare_text(text: &str, max_chars: usize) -> String {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| {
            let lw = w.to_lowercase();
            if lw.starts_with("http://") || lw.starts_with("https://") || lw.starts_with("www.") {
                "ссылка".to_string()
            } else {
                w.to_string()
            }
        })
        .collect();
    let joined = words.join(" ");
    let chars: Vec<char> = joined.chars().collect();
    if chars.len() > max_chars {
        chars[..max_chars].iter().collect()
    } else {
        joined
    }
}

/// Запускает поток озвучки. Сообщения кладутся через `enqueue`.
pub fn spawn(shared: Arc<Shared>) {
    let (tx, rx) = mpsc::channel::<Job>();
    *shared.tts_jobs.lock_ok() = Some(tx);
    thread::Builder::new()
        .name("tts".into())
        .spawn(move || run(shared, rx))
        .expect("не удалось запустить поток озвучки");
}

/// Ставит сообщение в очередь. false — очередь переполнена.
pub fn enqueue(shared: &Shared, job: Job) -> bool {
    if shared.tts_queue_len.load(Ordering::Relaxed) >= MAX_QUEUE {
        return false;
    }
    let guard = shared.tts_jobs.lock_ok();
    let Some(tx): Option<&Sender<Job>> = guard.as_ref() else { return false };
    if tx.send(job).is_ok() {
        shared.tts_queue_len.fetch_add(1, Ordering::Relaxed);
        true
    } else {
        false
    }
}

fn run(shared: Arc<Shared>, rx: Receiver<Job>) {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let mut synth = match Synth::new() {
        Ok(s) => s,
        Err(e) => {
            *shared.tts_error.lock_ok() = Some(format!("{e:#}"));
            return;
        }
    };

    loop {
        // «Очистить очередь».
        if shared.tts_clear.swap(false, Ordering::Relaxed) {
            while rx.try_recv().is_ok() {
                shared.tts_queue_len.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let job = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(j) => j,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        shared.tts_queue_len.fetch_sub(1, Ordering::Relaxed);
        shared.tts_skip.store(false, Ordering::Relaxed);

        synth.set_voice(&shared.tts_voice.lock_ok().clone());
        *shared.tts_now.lock_ok() = Some(job.label.clone());
        match synth.speak(&job.text) {
            Ok(samples) => {
                *shared.tts_error.lock_ok() = None;
                play(&shared, &samples);
            }
            Err(e) => *shared.tts_error.lock_ok() = Some(format!("Не удалось озвучить: {e:#}")),
        }
        *shared.tts_now.lock_ok() = None;
    }
}

/// Отдаёт звук в буфер звукового движка, пока он не проиграется (или «Пропустить»).
fn play(shared: &Shared, samples: &[f32]) {
    let mut pos = 0;
    loop {
        if shared.tts_skip.swap(false, Ordering::Relaxed) {
            shared.tts_flush.store(true, Ordering::Relaxed);
            return;
        }
        let mut guard = shared.tts_tx.lock_ok();
        let Some(tx) = guard.as_mut() else {
            // Звук не запущен — нечего ждать.
            return;
        };
        let free = tx.slots();
        if pos < samples.len() {
            let n = free.min(samples.len() - pos).min(4800);
            for &s in &samples[pos..pos + n] {
                let _ = tx.push(s);
            }
            pos += n;
        } else if free >= TTS_RING - 1 {
            // Всё отдано и проиграно.
            return;
        }
        drop(guard);
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_replaced_and_text_is_cut() {
        assert_eq!(prepare_text("смотри https://ya.ru вот", 100), "смотри ссылка вот");
        assert_eq!(prepare_text("  много    пробелов  ", 100), "много пробелов");
        assert_eq!(prepare_text("абвгдежзик", 5), "абвгд");
    }
}
