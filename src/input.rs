//! Turning typed text (from a phone keyboard) into key presses, and the keys a session may send.

/// evdev codes used here.
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_ENTER: u32 = 28;
pub const KEY_TAB: u32 = 15;
pub const KEY_SPACE: u32 = 57;

/// The evdev keycode and whether Shift is needed to type `c` on a US layout.
pub fn us_key(c: char) -> Option<(u32, bool)> {
    const LETTERS: [u32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    const DIGITS: [u32; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    Some(match c {
        'a'..='z' => (LETTERS[c as usize - 'a' as usize], false),
        'A'..='Z' => (LETTERS[c as usize - 'A' as usize], true),
        '0'..='9' => (DIGITS[c as usize - '0' as usize], false),
        ' ' => (KEY_SPACE, false),
        '\n' | '\r' => (KEY_ENTER, false),
        '\t' => (KEY_TAB, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        // Smart punctuation from phone keyboards, typed as its plain equivalent.
        '\u{2018}' | '\u{2019}' => (40, false),
        '\u{201c}' | '\u{201d}' => (40, true),
        '\u{2013}' | '\u{2014}' => (12, false),
        _ => return None,
    })
}

/// A session may press any key on a normal keyboard (evdev 1..=248) but nothing beyond, so a
/// client cannot reach power, sleep or other system keys.
pub fn key_allowed(code: u32) -> bool {
    (1..=248).contains(&code) && !matches!(code, 116 | 142 | 143 | 205)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_layout() {
        assert_eq!(us_key('a'), Some((30, false)));
        assert_eq!(us_key('Q'), Some((16, true)));
        assert_eq!(us_key('0'), Some((11, false)));
        assert_eq!(us_key('?'), Some((53, true)));
        assert_eq!(us_key('\u{2019}'), Some((40, false)));
        assert_eq!(us_key('é'), None);
    }

    #[test]
    fn power_keys_blocked() {
        assert!(key_allowed(30));
        assert!(!key_allowed(116)); // KEY_POWER
        assert!(!key_allowed(142)); // KEY_SLEEP
        assert!(!key_allowed(0));
        assert!(!key_allowed(0x110));
    }
}
