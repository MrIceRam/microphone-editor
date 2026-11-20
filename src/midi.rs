//! MIDI-клавиатура: подключение и разбор нот.
//!
//! Поток раз в секунду проверяет список MIDI-устройств: если пианино
//! подключили — подключается само, если выдернули — отпускает все клавиши.
//! Ноты идут сразу в три места: синтезатор (звук), привязки клавиш (игры)
//! и окно (подсветка экранного пианино).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use midir::{Ignore, MidiInput, MidiInputConnection};

use crate::state::LockExt;
use crate::state::Shared;

/// Включает/выключает «клавиши для игр» (нота-переключатель).
pub fn toggle_game_keys(shared: &Shared) {
    let on = !shared.keys_enabled.load(Ordering::Relaxed);
    shared.keys_enabled.store(on, Ordering::Relaxed);
    shared.keymapper.lock_ok().set_enabled(on);
    // Короткий сигнал в наушники: вверх — включено, вниз — выключено.
    shared.cue.store(if on { 1 } else { 2 }, Ordering::Relaxed);
}

/// Обрабатывает MIDI-сообщение. `from_device` = пришло с настоящего пианино
/// (только такие ноты нажимают клавиши — экранное пианино клавиш не жмёт).
pub fn handle_message(shared: &Shared, msg: &[u8], from_device: bool) {
    if msg.is_empty() {
        return;
    }
    let status = msg[0];
    if status < 0x80 || status >= 0xF0 {
        return; // системные сообщения (синхронизация и т.п.) не нужны
    }
    let d1 = msg.get(1).copied().unwrap_or(0) & 127;
    let d2 = msg.get(2).copied().unwrap_or(0) & 127;
    let kind = status & 0xF0;
    let is_toggle = shared.toggle_note.load(Ordering::Relaxed) == d1 as i16;

    match kind {
        0x90 if d2 > 0 => {
            if shared.learn_note.swap(false, Ordering::Relaxed) {
                // Режим «назначить ноту»: запоминаем ноту, клавишу не жмём.
                shared.learned_note.store(d1 as i16, Ordering::Relaxed);
            } else if is_toggle {
                // Нота-переключатель не звучит и ничего не нажимает.
                toggle_game_keys(shared);
                return;
            } else if from_device {
                shared.keymapper.lock_ok().note_on(d1);
            }
            shared.set_note_down(d1, true);
            shared.last_note.store(d1 as i16, Ordering::Relaxed);
            shared.last_velocity.store(d2, Ordering::Relaxed);
        }
        0x80 | 0x90 => {
            if is_toggle {
                return;
            }
            shared.set_note_down(d1, false);
            if from_device {
                shared.keymapper.lock_ok().note_off(d1);
            }
        }
        // Правая педаль (sustain, CC 64).
        0xB0 if d1 == 64 => {
            shared.pedal_down.store(d2 >= 64, Ordering::Relaxed);
            if from_device {
                shared.keymapper.lock_ok().pedal(d2 >= 64);
            }
        }
        _ => {}
    }

    // В синтезатор.
    if let Some(tx) = shared.piano_tx.lock_ok().as_mut() {
        let _ = tx.push([status, d1, d2]);
    }
}

#[derive(Default, Clone)]
pub struct MidiStatus {
    /// Название подключённого устройства.
    pub connected: Option<String>,
    /// Все найденные MIDI-устройства.
    pub ports: Vec<String>,
    pub error: Option<String>,
}

pub fn spawn(shared: Arc<Shared>) {
    // Ноты обрабатываются в своём потоке, а не в системном потоке MIDI-драйвера:
    // Windows запрещает вызывать оттуда почти всё (в том числе нажатия клавиш),
    // а любая ошибка там мгновенно закрывала бы программу.
    let (events_tx, events_rx) = mpsc::channel::<[u8; 3]>();
    let worker_shared = shared.clone();
    thread::Builder::new()
        .name("midi-events".into())
        .spawn(move || events_worker(worker_shared, events_rx))
        .expect("не удалось запустить поток нот");
    thread::Builder::new()
        .name("midi".into())
        .spawn(move || run(shared, events_tx))
        .expect("не удалось запустить MIDI-поток");
}

fn events_worker(shared: Arc<Shared>, rx: Receiver<[u8; 3]>) {
    for msg in rx {
        // Ошибка в обработке одной ноты не должна закрывать программу:
        // она запишется в crash.log, а следующие ноты продолжат работать.
        let _ = catch_unwind(AssertUnwindSafe(|| handle_message(&shared, &msg, true)));
    }
}

fn port_names() -> Vec<String> {
    let Ok(input) = MidiInput::new("Microphone Editor (список)") else {
        return Vec::new();
    };
    input
        .ports()
        .iter()
        .filter_map(|p| input.port_name(p).ok())
        .collect()
}

fn choose_port(ports: &[String], wanted: &str) -> Option<usize> {
    let w = wanted.trim().to_lowercase();
    if w.is_empty() {
        return if ports.is_empty() { None } else { Some(0) };
    }
    ports
        .iter()
        .position(|p| p.to_lowercase() == w)
        .or_else(|| ports.iter().position(|p| p.to_lowercase().contains(&w)))
}

fn run(shared: Arc<Shared>, events: Sender<[u8; 3]>) {
    let mut conn: Option<(MidiInputConnection<()>, String, String)> = None; // (соединение, имя, запрошенное)

    loop {
        let ports = port_names();
        let wanted = shared.midi_device.lock_ok().clone();

        // Отключаемся, если устройство пропало или в настройках выбрано другое.
        let drop_it = match &conn {
            Some((_, name, asked)) => !ports.contains(name) || *asked != wanted,
            None => false,
        };
        if drop_it {
            if let Some((c, _, _)) = conn.take() {
                c.close();
            }
            shared.keymapper.lock_ok().release_all();
            shared.clear_notes();
        }

        let mut error = None;
        if conn.is_none() {
            if let Some(idx) = choose_port(&ports, &wanted) {
                match connect(&ports[idx], events.clone()) {
                    Ok(c) => conn = Some((c, ports[idx].clone(), wanted.clone())),
                    Err(e) => error = Some(e),
                }
            }
        }

        *shared.midi_status.lock_ok() = MidiStatus {
            connected: conn.as_ref().map(|(_, n, _)| n.clone()),
            ports,
            error,
        };
        thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_note_switches_game_keys_and_stays_silent() {
        let shared = Shared::new();
        shared.toggle_note.store(96, Ordering::Relaxed);

        handle_message(&shared, &[0x90, 96, 100], false);
        assert!(shared.keys_enabled.load(Ordering::Relaxed));
        assert_eq!(shared.cue.swap(0, Ordering::Relaxed), 1, "сигнал «включено»");
        assert!(!shared.note_is_down(96), "переключатель не должен подсвечиваться как нота");
        handle_message(&shared, &[0x80, 96, 0], false);

        handle_message(&shared, &[0x90, 96, 100], false);
        assert!(!shared.keys_enabled.load(Ordering::Relaxed));
        assert_eq!(shared.cue.swap(0, Ordering::Relaxed), 2, "сигнал «выключено»");

        // Обычная нота работает как раньше.
        handle_message(&shared, &[0x90, 60, 100], false);
        assert!(shared.note_is_down(60));
    }

    #[test]
    fn learning_takes_priority_over_toggle() {
        let shared = Shared::new();
        shared.toggle_note.store(96, Ordering::Relaxed);
        shared.learn_note.store(true, Ordering::Relaxed);
        handle_message(&shared, &[0x90, 96, 100], false);
        assert_eq!(shared.learned_note.load(Ordering::Relaxed), 96);
        assert!(!shared.keys_enabled.load(Ordering::Relaxed));
    }
}

fn connect(name: &str, events: Sender<[u8; 3]>) -> Result<MidiInputConnection<()>, String> {
    let mut input = MidiInput::new("Microphone Editor").map_err(|e| e.to_string())?;
    input.ignore(Ignore::All);
    let ports = input.ports();
    let port = ports
        .iter()
        .find(|p| input.port_name(p).ok().as_deref() == Some(name))
        .ok_or_else(|| format!("устройство «{name}» пропало"))?
        .clone();
    input
        .connect(
            &port,
            "microphone-editor-in",
            // Здесь — системный поток драйвера: только кладём ноту в очередь и выходим.
            move |_, msg, _| {
                if let Some(&status) = msg.first() {
                    let m = [status, msg.get(1).copied().unwrap_or(0), msg.get(2).copied().unwrap_or(0)];
                    let _ = events.send(m);
                }
            },
            (),
        )
        .map_err(|e| format!("не удалось подключиться к «{name}»: {e} (может, его заняла другая программа?)"))
}
