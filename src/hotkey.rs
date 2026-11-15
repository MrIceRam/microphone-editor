//! Глобальная горячая клавиша лага.
//!
//! Клавиша опрашивается каждые 5 мс через GetAsyncKeyState. Так она
//! работает в любой игре и окне, не «съедает» нажатие (игра его тоже видит)
//! и можно отличить «держу» от «отпустил».

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

use crate::keys;
use crate::state::{CAPTURE_CANCELLED, Shared};

fn is_down(vk: u16) -> bool {
    // Старший бит = клавиша сейчас зажата.
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

pub fn spawn(shared: Arc<Shared>) {
    thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || run(shared))
        .expect("не удалось запустить поток горячих клавиш");
}

fn run(shared: Arc<Shared>) {
    let mut prev_down = false;
    // При назначении сначала ждём, пока отпустят все клавиши.
    let mut capture_armed = false;

    loop {
        thread::sleep(Duration::from_millis(5));

        if shared.capturing.load(Ordering::Relaxed) {
            if !capture_armed {
                capture_armed = !keys::capturable_keys().any(is_down) && !is_down(keys::VK_ESCAPE);
                continue;
            }
            if is_down(keys::VK_ESCAPE) {
                shared.captured_vk.store(CAPTURE_CANCELLED, Ordering::Relaxed);
                shared.capturing.store(false, Ordering::Relaxed);
                capture_armed = false;
                continue;
            }
            if let Some(vk) = keys::capturable_keys().find(|&vk| is_down(vk)) {
                shared.captured_vk.store(vk, Ordering::Relaxed);
                shared.hotkey_vk.store(vk, Ordering::Relaxed);
                shared.capturing.store(false, Ordering::Relaxed);
                capture_armed = false;
                // Чтобы то же нажатие сразу не включило лаг.
                prev_down = true;
            }
            continue;
        }
        capture_armed = false;

        let vk = shared.hotkey_vk.load(Ordering::Relaxed);
        let down = vk != 0 && is_down(vk);
        if shared.hotkey_toggle.load(Ordering::Relaxed) {
            if down && !prev_down {
                shared.lag_latched.fetch_xor(true, Ordering::Relaxed);
            }
            shared.lag_held.store(false, Ordering::Relaxed);
        } else {
            shared.lag_held.store(down, Ordering::Relaxed);
        }
        prev_down = down;
    }
}
