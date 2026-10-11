//! Keyboard keys from the desktop window, for development typing.
//!
//! The pad has no keyboard (ADR-118 §2); on the Mac window the console
//! host accepts typed utterances so it can be tested without a
//! microphone. The window backend reports each key press as a sensor
//! event: a printable character on [`SENSOR_KEY_CHAR`], or a control
//! key by name on [`SENSOR_KEY_NAMED`]. Mapping the SDL key name is
//! pure, so it is tested here without SDL.

/// A printable character was typed. Value is `SensorValue::Text(ch)`;
/// the space bar reports `" "` here as well as `ptt.down` / `ptt.up`,
/// and the app decides which it means.
pub const SENSOR_KEY_CHAR: &str = "key.char";
/// A control key went down. Value is `SensorValue::Text(name)` with one
/// of [`KEY_RETURN`], [`KEY_BACKSPACE`] or [`KEY_ESCAPE`].
pub const SENSOR_KEY_NAMED: &str = "key.named";

pub const KEY_RETURN: &str = "Return";
pub const KEY_BACKSPACE: &str = "Backspace";
pub const KEY_ESCAPE: &str = "Escape";

/// Resolve an SDL key name (`Keycode::name()`) and the shift state to a
/// sensor name and value, or `None` for keys the console has no use for
/// (function keys, arrows, modifiers).
pub fn key_sensor(name: &str, shift: bool) -> Option<(&'static str, String)> {
    match name {
        "Return" | "Return2" | "Keypad Enter" => Some((SENSOR_KEY_NAMED, KEY_RETURN.to_string())),
        "Backspace" => Some((SENSOR_KEY_NAMED, KEY_BACKSPACE.to_string())),
        "Escape" => Some((SENSOR_KEY_NAMED, KEY_ESCAPE.to_string())),
        "Space" => Some((SENSOR_KEY_CHAR, " ".to_string())),
        _ => {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() || !c.is_ascii_graphic() {
                return None;
            }
            let out = if c.is_ascii_alphabetic() {
                if shift {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            } else if shift {
                shifted_symbol(c)
            } else {
                c
            };
            Some((SENSOR_KEY_CHAR, out.to_string()))
        }
    }
}

/// US layout: the symbol on the upper half of a key.
fn shifted_symbol(c: char) -> char {
    match c {
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        '`' => '~',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_follow_shift_and_symbols_shift_on_us_layout() {
        assert_eq!(key_sensor("A", false), Some((SENSOR_KEY_CHAR, "a".into())));
        assert_eq!(key_sensor("A", true), Some((SENSOR_KEY_CHAR, "A".into())));
        assert_eq!(key_sensor("1", false), Some((SENSOR_KEY_CHAR, "1".into())));
        assert_eq!(key_sensor("1", true), Some((SENSOR_KEY_CHAR, "!".into())));
        assert_eq!(key_sensor("/", true), Some((SENSOR_KEY_CHAR, "?".into())));
        assert_eq!(key_sensor("Space", false), Some((SENSOR_KEY_CHAR, " ".into())));
    }

    #[test]
    fn control_keys_are_named_and_the_rest_are_dropped() {
        assert_eq!(key_sensor("Return", false), Some((SENSOR_KEY_NAMED, "Return".into())));
        assert_eq!(key_sensor("Keypad Enter", false), Some((SENSOR_KEY_NAMED, "Return".into())));
        assert_eq!(key_sensor("Backspace", true), Some((SENSOR_KEY_NAMED, "Backspace".into())));
        assert_eq!(key_sensor("Escape", false), Some((SENSOR_KEY_NAMED, "Escape".into())));
        assert_eq!(key_sensor("F1", false), None);
        assert_eq!(key_sensor("Left Shift", false), None);
        assert_eq!(key_sensor("Up", false), None);
    }
}
