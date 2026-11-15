//! Нажатия клавиш по нотам (для игр).
//!
//! Нажатия отправляются через SendInput со скан-кодами — так игры видят их
//! как нажатия настоящей клавиатуры. Нота нажата = клавиша зажата, нота
//! отпущена = клавиша отпущена. Одна нота = одна клавиша (никаких макросов).

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
    MOUSEINPUT, MapVirtualKeyW, SendInput,
};

/// Нажимает (`down = true`) или отпускает клавишу / кнопку мыши с кодом `vk`.
pub fn send_key(vk: u16, down: bool) {
    let input = match vk {
        0x01 | 0x02 | 0x04 | 0x05 | 0x06 => mouse_input(vk, down),
        _ => keyboard_input(vk, down),
    };
    unsafe {
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

fn keyboard_input(vk: u16, down: bool) -> INPUT {
    let sc = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) };
    let mut flags = KEYEVENTF_SCANCODE;
    // Расширенные клавиши (стрелки, правый Ctrl/Alt, Insert/Delete...) имеют префикс E0.
    if sc & 0xFF00 == 0xE000 || sc & 0xFF00 == 0xE100 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: (sc & 0xFF) as u16,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_input(vk: u16, down: bool) -> INPUT {
    let (flags, data) = match (vk, down) {
        (0x01, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (0x01, false) => (MOUSEEVENTF_LEFTUP, 0),
        (0x02, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (0x02, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (0x04, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (0x04, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (0x05, true) => (MOUSEEVENTF_XDOWN, 1),
        (0x05, false) => (MOUSEEVENTF_XUP, 1),
        (_, true) => (MOUSEEVENTF_XDOWN, 2),
        (_, false) => (MOUSEEVENTF_XUP, 2),
    };
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Кто реально нажимает клавиши (в тестах подменяется).
pub type KeySender = Box<dyn FnMut(u16, bool) + Send>;

/// Привязки «нота → клавиша» и учёт зажатых клавиш.
pub struct KeyMapper {
    /// Нота -> код клавиши (0 = не назначено).
    pub map: [u16; 128],
    /// Клавиша для правой педали (0 = нет).
    pub pedal_vk: u16,
    enabled: bool,
    /// Сколько нот сейчас держат каждую клавишу.
    held: [u16; 256],
    /// Какую клавишу нажала каждая нота (чтобы отпустить ту же, даже если привязку поменяли).
    active: [u16; 128],
    pedal_active: u16,
    sender: KeySender,
}

impl KeyMapper {
    pub fn new() -> Self {
        Self::with_sender(Box::new(send_key))
    }

    pub fn with_sender(sender: KeySender) -> Self {
        Self {
            map: [0; 128],
            pedal_vk: 0,
            enabled: false,
            held: [0; 256],
            active: [0; 128],
            pedal_active: 0,
            sender,
        }
    }

    pub fn set_enabled(&mut self, on: bool) {
        if !on {
            self.release_all();
        }
        self.enabled = on;
    }

    pub fn note_on(&mut self, note: u8) {
        let note = note as usize & 127;
        if !self.enabled || self.active[note] != 0 {
            return;
        }
        let vk = self.map[note];
        if vk != 0 {
            self.active[note] = vk;
            self.press(vk);
        }
    }

    pub fn note_off(&mut self, note: u8) {
        let note = note as usize & 127;
        let vk = std::mem::take(&mut self.active[note]);
        if vk != 0 {
            self.release(vk);
        }
    }

    pub fn pedal(&mut self, down: bool) {
        if down {
            if self.enabled && self.pedal_active == 0 && self.pedal_vk != 0 {
                self.pedal_active = self.pedal_vk;
                self.press(self.pedal_vk);
            }
        } else {
            let vk = std::mem::take(&mut self.pedal_active);
            if vk != 0 {
                self.release(vk);
            }
        }
    }

    /// Отпускает всё, что сейчас зажато (при выключении, отключении пианино, выходе).
    pub fn release_all(&mut self) {
        for note in 0..128u8 {
            self.note_off(note);
        }
        self.pedal(false);
    }

    fn press(&mut self, vk: u16) {
        let h = &mut self.held[vk as usize & 0xFF];
        *h += 1;
        if *h == 1 {
            (self.sender)(vk, true);
        }
    }

    fn release(&mut self, vk: u16) {
        let h = &mut self.held[vk as usize & 0xFF];
        if *h == 0 {
            return;
        }
        *h -= 1;
        if *h == 0 {
            (self.sender)(vk, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn mapper() -> (KeyMapper, Arc<Mutex<Vec<(u16, bool)>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        let mut m = KeyMapper::with_sender(Box::new(move |vk, down| l.lock().unwrap().push((vk, down))));
        m.map[60] = 0x51; // C4 -> Q
        m.map[62] = 0x51; // D4 -> тоже Q
        m.map[64] = 0x57; // E4 -> W
        m.pedal_vk = 0xA0;
        m.set_enabled(true);
        (m, log)
    }

    #[test]
    fn press_and_release() {
        let (mut m, log) = mapper();
        m.note_on(60);
        m.note_on(60); // повтор — без второго нажатия
        m.note_off(60);
        m.note_on(61); // не назначена
        assert_eq!(*log.lock().unwrap(), vec![(0x51, true), (0x51, false)]);
    }

    #[test]
    fn shared_key_released_after_last_note() {
        let (mut m, log) = mapper();
        m.note_on(60);
        m.note_on(62);
        m.note_off(60);
        assert_eq!(log.lock().unwrap().len(), 1, "клавиша должна держаться, пока зажата вторая нота");
        m.note_off(62);
        assert_eq!(*log.lock().unwrap(), vec![(0x51, true), (0x51, false)]);
    }

    #[test]
    fn remap_while_held_releases_original_key() {
        let (mut m, log) = mapper();
        m.note_on(64);
        m.map[64] = 0x45; // поменяли на E
        m.note_off(64);
        assert_eq!(*log.lock().unwrap(), vec![(0x57, true), (0x57, false)]);
    }

    #[test]
    fn disable_releases_everything() {
        let (mut m, log) = mapper();
        m.note_on(60);
        m.pedal(true);
        m.set_enabled(false);
        m.note_on(64);
        let l = log.lock().unwrap();
        assert!(l.contains(&(0x51, false)) && l.contains(&(0xA0, false)));
        assert!(!l.contains(&(0x57, true)));
    }
}
