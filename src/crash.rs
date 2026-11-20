//! Журнал падений.
//!
//! У программы нет консоли, поэтому без журнала причина падения просто
//! пропадает. При любой ошибке (панике) в любом потоке сюда пишется:
//! время, поток, место в коде и стек вызовов. При следующем запуске окно
//! покажет, что в прошлый раз что-то сломалось.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

fn dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn log_path() -> PathBuf {
    dir().join("crash.log")
}

fn pending_path() -> PathBuf {
    dir().join("crash.pending")
}

/// Ставит перехватчик паник. Вызывается в самом начале программы.
pub fn install() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("без имени").to_string();
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".into());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "?".into());
        let when = chrono_like_now();
        let backtrace = std::backtrace::Backtrace::force_capture();
        let entry = format!(
            "==== {when} | версия {} ====\nПоток: {name}\nГде: {location}\nОшибка: {message}\n{backtrace}\n\n",
            env!("CARGO_PKG_VERSION")
        );
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
            let _ = f.write_all(entry.as_bytes());
        }
        let _ = fs::write(pending_path(), format!("{name}: {message} ({location})"));
        default_hook(info);
    }));
}

/// Если в прошлый раз программа упала — короткое описание (один раз).
pub fn take_pending() -> Option<String> {
    let text = fs::read_to_string(pending_path()).ok()?;
    let _ = fs::remove_file(pending_path());
    Some(text)
}

/// Время без внешних библиотек: секунды с 1970 года в UTC → «2026-11-20 14:03:05 UTC».
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Алгоритм перевода дней в дату (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn date_format_looks_right() {
        let s = super::chrono_like_now();
        assert!(s.starts_with("20") && s.ends_with("UTC") && s.len() == 23, "{s}");
    }
}
