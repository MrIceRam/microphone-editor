//! MIDI-клавиатура: подключение и разбор нот.
//!
//! Поток раз в секунду проверяет список MIDI-устройств: если пианино
//! подключили — подключается само, если выдернули — отпускает все клавиши.
//! Ноты идут сразу в три места: синтезатор (звук), привязки клавиш (игры)
//! и окно (подсветка экранного пианино).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use midir::{Ignore, MidiInput, MidiInputConnection};

use crate::state::Shared;

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

    match kind {
        0x90 if d2 > 0 => {
            shared.set_note_down(d1, true);
            shared.last_note.store(d1 as i16, Ordering::Relaxed);
            shared.last_velocity.store(d2, Ordering::Relaxed);
            if shared.learn_note.swap(false, Ordering::Relaxed) {
                // Режим «назначить ноту»: запоминаем ноту и клавишу не жмём.
                shared.learned_note.store(d1 as i16, Ordering::Relaxed);
            } else if from_device {
                shared.keymapper.lock().unwrap().note_on(d1);
            }
        }
        0x80 | 0x90 => {
            shared.set_note_down(d1, false);
            if from_device {
                shared.keymapper.lock().unwrap().note_off(d1);
            }
        }
        // Правая педаль (sustain, CC 64).
        0xB0 if d1 == 64 => {
            shared.pedal_down.store(d2 >= 64, Ordering::Relaxed);
            if from_device {
                shared.keymapper.lock().unwrap().pedal(d2 >= 64);
            }
        }
        _ => {}
    }

    // В синтезатор.
    if let Some(tx) = shared.piano_tx.lock().unwrap().as_mut() {
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
    thread::Builder::new()
        .name("midi".into())
        .spawn(move || run(shared))
        .expect("не удалось запустить MIDI-поток");
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

fn run(shared: Arc<Shared>) {
    let mut conn: Option<(MidiInputConnection<()>, String, String)> = None; // (соединение, имя, запрошенное)

    loop {
        let ports = port_names();
        let wanted = shared.midi_device.lock().unwrap().clone();

        // Отключаемся, если устройство пропало или в настройках выбрано другое.
        let drop_it = match &conn {
            Some((_, name, asked)) => !ports.contains(name) || *asked != wanted,
            None => false,
        };
        if drop_it {
            if let Some((c, _, _)) = conn.take() {
                c.close();
            }
            shared.keymapper.lock().unwrap().release_all();
            shared.clear_notes();
        }

        let mut error = None;
        if conn.is_none() {
            if let Some(idx) = choose_port(&ports, &wanted) {
                match connect(&shared, &ports[idx]) {
                    Ok(c) => conn = Some((c, ports[idx].clone(), wanted.clone())),
                    Err(e) => error = Some(e),
                }
            }
        }

        *shared.midi_status.lock().unwrap() = MidiStatus {
            connected: conn.as_ref().map(|(_, n, _)| n.clone()),
            ports,
            error,
        };
        thread::sleep(Duration::from_secs(1));
    }
}

fn connect(shared: &Arc<Shared>, name: &str) -> Result<MidiInputConnection<()>, String> {
    let mut input = MidiInput::new("Microphone Editor").map_err(|e| e.to_string())?;
    input.ignore(Ignore::All);
    let ports = input.ports();
    let port = ports
        .iter()
        .find(|p| input.port_name(p).ok().as_deref() == Some(name))
        .ok_or_else(|| format!("устройство «{name}» пропало"))?
        .clone();
    let sh = shared.clone();
    input
        .connect(&port, "microphone-editor-in", move |_, msg, _| handle_message(&sh, msg, true), ())
        .map_err(|e| format!("не удалось подключиться к «{name}»: {e} (может, его заняла другая программа?)"))
}
