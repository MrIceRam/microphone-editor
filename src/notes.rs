//! Названия нот <-> номера MIDI. 60 = C4 = «до» первой октавы (средняя «до»).

const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
const NAMES_RU: [&str; 12] = ["до", "до#", "ре", "ре#", "ми", "фа", "фа#", "соль", "соль#", "ля", "ля#", "си"];

/// 60 -> "C4", 61 -> "C#4".
pub fn note_name(n: u8) -> String {
    format!("{}{}", NAMES[(n % 12) as usize], n as i32 / 12 - 1)
}

/// 60 -> "до".
pub fn note_name_ru(n: u8) -> &'static str {
    NAMES_RU[(n % 12) as usize]
}

/// Чёрная ли клавиша.
pub fn is_black(n: u8) -> bool {
    matches!(n % 12, 1 | 3 | 6 | 8 | 10)
}

/// "C4", "c#4", "Db4", "A-1" -> номер MIDI.
pub fn parse_note(s: &str) -> Option<u8> {
    let s = s.trim();
    let mut chars = s.chars();
    let letter = chars.next()?.to_ascii_uppercase();
    let base: i32 = match letter {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let rest: String = chars.collect();
    let (shift, octave_str) = if let Some(r) = rest.strip_prefix('#') {
        (1, r)
    } else if let Some(r) = rest.strip_prefix('b') {
        (-1, r)
    } else {
        (0, rest.as_str())
    };
    let octave: i32 = octave_str.parse().ok()?;
    let n = (octave + 1) * 12 + base + shift;
    (0..=127).contains(&n).then_some(n as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_roundtrip() {
        for n in 0..=127u8 {
            assert_eq!(parse_note(&note_name(n)), Some(n), "{n}");
        }
        assert_eq!(note_name(60), "C4");
        assert_eq!(parse_note("db4"), Some(61));
        assert_eq!(parse_note("A0"), Some(21));
        assert_eq!(parse_note("H4"), None);
        assert_eq!(parse_note("C"), None);
        assert!(is_black(61) && !is_black(60));
    }
}
