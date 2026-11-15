//! Названия клавиш <-> коды клавиш Windows (Virtual-Key codes).

/// (код, название). Первое название для кода — «главное», оно сохраняется в настройки.
const KEYS: &[(u16, &str)] = &[
    (0x04, "Mouse3"),
    (0x05, "Mouse4"),
    (0x06, "Mouse5"),
    (0x08, "Backspace"),
    (0x09, "Tab"),
    (0x0D, "Enter"),
    (0x10, "Shift"),
    (0x11, "Ctrl"),
    (0x12, "Alt"),
    (0x13, "Pause"),
    (0x14, "CapsLock"),
    (0x1B, "Esc"),
    (0x20, "Space"),
    (0x21, "PageUp"),
    (0x22, "PageDown"),
    (0x23, "End"),
    (0x24, "Home"),
    (0x25, "Left"),
    (0x26, "Up"),
    (0x27, "Right"),
    (0x28, "Down"),
    (0x2C, "PrintScreen"),
    (0x2D, "Insert"),
    (0x2E, "Delete"),
    (0x6A, "NumpadMultiply"),
    (0x6B, "NumpadAdd"),
    (0x6D, "NumpadSubtract"),
    (0x6E, "NumpadDecimal"),
    (0x6F, "NumpadDivide"),
    (0x90, "NumLock"),
    (0x91, "ScrollLock"),
    (0xA0, "LShift"),
    (0xA1, "RShift"),
    (0xA2, "LCtrl"),
    (0xA3, "RCtrl"),
    (0xA4, "LAlt"),
    (0xA5, "RAlt"),
    (0xBA, "Semicolon"),
    (0xBB, "Equals"),
    (0xBC, "Comma"),
    (0xBD, "Minus"),
    (0xBE, "Period"),
    (0xBF, "Slash"),
    (0xC0, "Tilde"),
    (0xDB, "LBracket"),
    (0xDC, "Backslash"),
    (0xDD, "RBracket"),
    (0xDE, "Quote"),
];

/// Код клавиши по названию ("F8", "x", "Mouse4"...). `None` — неизвестная клавиша.
pub fn parse_key(name: &str) -> Option<u16> {
    let n = name.trim();
    if n.is_empty() {
        return None;
    }
    let upper = n.to_ascii_uppercase();

    // Буквы и цифры: "A".."Z", "0".."9"
    if upper.len() == 1 {
        let c = upper.as_bytes()[0];
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u16);
        }
    }
    // F1..F24
    if let Some(num) = upper.strip_prefix('F') {
        if let Ok(k) = num.parse::<u16>() {
            if (1..=24).contains(&k) {
                return Some(0x70 + k - 1);
            }
        }
    }
    // Numpad0..Numpad9
    if let Some(num) = upper.strip_prefix("NUMPAD") {
        if let Ok(k) = num.parse::<u16>() {
            if k <= 9 {
                return Some(0x60 + k);
            }
        }
    }
    KEYS.iter()
        .find(|(_, kname)| kname.eq_ignore_ascii_case(n))
        .map(|(vk, _)| *vk)
}

/// Название клавиши по коду.
pub fn key_name(vk: u16) -> String {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => (vk as u8 as char).to_string(),
        0x60..=0x69 => format!("Numpad{}", vk - 0x60),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        _ => KEYS
            .iter()
            .find(|(k, _)| *k == vk)
            .map(|(_, n)| n.to_string())
            .unwrap_or_else(|| format!("0x{vk:02X}")),
    }
}

/// Коды, которые проверяются при назначении клавиши.
/// Левая и правая кнопки мыши не назначаются — ими кликают по окну и играют.
/// Общие Shift/Ctrl/Alt пропускаются, чтобы ловились конкретные LShift/RCtrl и т.д.
pub fn capturable_keys() -> impl Iterator<Item = u16> {
    (0x03u16..=0xFE).filter(|vk| !matches!(vk, 0x10 | 0x11 | 0x12 | 0x1B))
}

pub const VK_ESCAPE: u16 = 0x1B;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for name in ["F8", "X", "7", "Mouse4", "CapsLock", "Numpad3", "LAlt", "Tilde", "F24"] {
            let vk = parse_key(name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(key_name(vk), name);
        }
        assert_eq!(parse_key("f8"), Some(0x77));
        assert_eq!(parse_key("mouse5"), Some(0x06));
        assert_eq!(parse_key(""), None);
        assert_eq!(parse_key("Blah"), None);
    }
}
