//! Программа запускается только один раз.
//!
//! Она живёт в трее, и её легко запустить повторно, забыв про уже
//! работающую копию. Тогда в виртуальный микрофон шли бы два голоса.
//! Поэтому вторая копия просто просит первую показать окно и выходит.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};

use crate::state::Shared;

const MUTEX_NAME: &str = "Local\\MicrophoneEditor-single-instance";
const EVENT_NAME: &str = "Local\\MicrophoneEditor-show-window";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// true — мы первая копия. false — программа уже запущена (её окно попросили показаться).
pub fn acquire() -> bool {
    let name = wide(MUTEX_NAME);
    // Мьютекс живёт до конца процесса — специально не закрываем.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if handle.is_null() || !already {
        return true;
    }
    let ev_name = wide(EVENT_NAME);
    let ev = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, ev_name.as_ptr()) };
    if !ev.is_null() {
        unsafe { SetEvent(ev) };
    }
    false
}

/// Ждёт просьб других копий «покажи окно».
pub fn listen(shared: Arc<Shared>) {
    let ev_name = wide(EVENT_NAME);
    // Автосброс: одно SetEvent = одно пробуждение.
    let ev = unsafe { CreateEventW(std::ptr::null(), 0, 0, ev_name.as_ptr()) };
    if ev.is_null() {
        return;
    }
    let ev = ev as usize;
    thread::Builder::new()
        .name("single-instance".into())
        .spawn(move || loop {
            unsafe { WaitForSingleObject(ev as _, INFINITE) };
            shared.show_request.store(true, Ordering::Relaxed);
            if let Some(ctx) = shared.egui_ctx.get() {
                ctx.request_repaint();
            }
        })
        .ok();
}
