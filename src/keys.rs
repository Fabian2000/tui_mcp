//! Translate high-level key names + modifiers into the byte sequences a
//! terminal application expects to read on its stdin.

/// Modifier flags. Combined into the xterm modifier parameter (1 + bitmask).
#[derive(Default, Clone, Copy)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    pub fn from_list(list: &[String]) -> Self {
        let mut m = Mods::default();
        for s in list {
            match s.to_ascii_lowercase().as_str() {
                "shift" => m.shift = true,
                "alt" | "meta" | "option" => m.alt = true,
                "ctrl" | "control" => m.ctrl = true,
                _ => {}
            }
        }
        m
    }

    fn any(self) -> bool {
        self.shift || self.alt || self.ctrl
    }

    /// xterm modifier parameter: 1 + (shift=1, alt=2, ctrl=4).
    fn xterm_param(self) -> u8 {
        1 + u8::from(self.shift) + (u8::from(self.alt) << 1) + (u8::from(self.ctrl) << 2)
    }
}

/// A CSI sequence that ends in a final letter (arrows, Home/End, F1-F4).
/// With modifiers it becomes `ESC [ 1 ; <param> <final>`.
fn csi_letter(final_byte: u8, mods: Mods) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[1;{}{}", mods.xterm_param(), final_byte as char).into_bytes()
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

/// A CSI sequence of the `ESC [ <num> ~` form (Insert, Delete, `PageUp`, F5-F12).
fn csi_tilde(num: u32, mods: Mods) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[{num};{}~", mods.xterm_param()).into_bytes()
    } else {
        format!("\x1b[{num}~").into_bytes()
    }
}

/// Translate a named key into bytes. Returns `None` for unknown names.
pub fn key_to_bytes(name: &str, mods: Mods) -> Option<Vec<u8>> {
    let lower = name.to_ascii_lowercase();
    let bytes = match lower.as_str() {
        "enter" | "return" | "cr" => vec![b'\r'],
        "tab" => {
            if mods.shift {
                b"\x1b[Z".to_vec() // back-tab
            } else {
                vec![b'\t']
            }
        }
        "escape" | "esc" => vec![0x1b],
        "backspace" | "bs" => vec![0x7f],
        "space" => prefix_alt(vec![b' '], mods),
        "delete" | "del" => csi_tilde(3, mods),
        "insert" | "ins" => csi_tilde(2, mods),
        "home" => csi_letter(b'H', mods),
        "end" => csi_letter(b'F', mods),
        "pageup" | "pgup" => csi_tilde(5, mods),
        "pagedown" | "pgdn" => csi_tilde(6, mods),
        "up" => csi_letter(b'A', mods),
        "down" => csi_letter(b'B', mods),
        "right" => csi_letter(b'C', mods),
        "left" => csi_letter(b'D', mods),
        "f1" => csi_ss3_or_mod(b'P', mods),
        "f2" => csi_ss3_or_mod(b'Q', mods),
        "f3" => csi_ss3_or_mod(b'R', mods),
        "f4" => csi_ss3_or_mod(b'S', mods),
        "f5" => csi_tilde(15, mods),
        "f6" => csi_tilde(17, mods),
        "f7" => csi_tilde(18, mods),
        "f8" => csi_tilde(19, mods),
        "f9" => csi_tilde(20, mods),
        "f10" => csi_tilde(21, mods),
        "f11" => csi_tilde(23, mods),
        "f12" => csi_tilde(24, mods),
        _ => {
            // Single printable character: apply ctrl/alt.
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None; // multi-char, not a known key name
            }
            return Some(char_with_mods(c, mods));
        }
    };
    Some(bytes)
}

/// Translate a named key into bytes for a piped session (no terminal).
/// Same as `key_to_bytes`, except Enter.
pub fn piped_key_to_bytes(name: &str, mods: Mods) -> Option<Vec<u8>> {
    match name.to_ascii_lowercase().as_str() {
        // A piped program reads lines, and a line ends in `\n`. The `\r` a
        // terminal's Enter sends becomes `\n` only through a tty's line
        // discipline (ICRNL), which pipes don't have, so it never completes a
        // line there. Modifiers are ignored, as in `key_to_bytes`.
        //
        // Intentionally `\n` on Windows too, not the Windows-specific `\r\n`:
        // Windows line readers (C runtime text mode, Python, .NET `ReadLine`,
        // cmd's `set /p`) accept a bare `\n`, while `\r\n` would leave a stray
        // `\r` in Unix-born programs started piped from Windows (Git Bash,
        // WSL). If a Windows program turns out to need it, switch to `\r\n`
        // on Windows here. `cr` still sends `\r`.
        "enter" | "return" => Some(vec![b'\n']),
        _ => key_to_bytes(name, mods),
    }
}

/// F1-F4 without mods use SS3 (`ESC O P` etc.); with mods use the CSI form.
fn csi_ss3_or_mod(final_byte: u8, mods: Mods) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[1;{}{}", mods.xterm_param(), final_byte as char).into_bytes()
    } else {
        vec![0x1b, b'O', final_byte]
    }
}

/// Apply ctrl (collapse to control code) and alt (ESC prefix) to a character.
fn char_with_mods(c: char, mods: Mods) -> Vec<u8> {
    let mut out = Vec::new();
    if mods.ctrl && c.is_ascii() {
        let upper = c.to_ascii_uppercase() as u8;
        // Ctrl maps @,A-Z,[,\,],^,_ to 0x00-0x1f.
        if (b'@'..=b'_').contains(&upper) {
            out.push(upper & 0x1f);
        } else if c.is_ascii_alphabetic() {
            out.push((c.to_ascii_uppercase() as u8) & 0x1f);
        } else {
            out.push(c as u8);
        }
    } else {
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
    prefix_alt(out, mods)
}

/// Alt is sent as a leading ESC byte.
fn prefix_alt(bytes: Vec<u8>, mods: Mods) -> Vec<u8> {
    if mods.alt {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(&bytes);
        out
    } else {
        bytes
    }
}

/// The kitty modifier parameter: 1 + (shift=1, alt=2, ctrl=4).
fn kitty_mods(m: Mods) -> u32 {
    1 + u32::from(m.shift) + (u32::from(m.alt) << 1) + (u32::from(m.ctrl) << 2)
}

/// The kitty key code for a key name, or `None` for keys we leave to the legacy
/// (xterm) encoding: arrows, function keys, Home/End, and so on, which kitty keeps
/// in their CSI-letter / CSI-tilde forms.
fn kitty_keycode(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "enter" | "return" | "cr" => 13,
        "tab" => 9,
        "escape" | "esc" => 27,
        "backspace" | "bs" => 127,
        "space" => 32,
        _ => {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None; // multi-char named key we don't map
            }
            c.to_lowercase().next().unwrap_or(c) as u32
        }
    })
}

/// Encode a key press in the kitty keyboard protocol, or `None` to fall back to
/// [`key_to_bytes`]. Returns `None` when the protocol is disabled (`flags == 0`),
/// for keys handled by the legacy encoder, or for unmodified text keys unless
/// "report all keys as escape codes" (bit 3) is set, matching kitty semantics
/// where plain typing still arrives as text.
pub fn kitty_encode(name: &str, mods: Mods, flags: u8) -> Option<Vec<u8>> {
    if flags == 0 {
        return None;
    }
    let code = kitty_keycode(name)?;
    let report_all = flags & 0b1000 != 0;
    let disambiguate = report_all || mods.ctrl || mods.alt;
    if !disambiguate {
        return None;
    }
    let m = kitty_mods(mods);
    let seq = if m == 1 {
        format!("\x1b[{code}u")
    } else {
        format!("\x1b[{code};{m}u")
    };
    Some(seq.into_bytes())
}

/// Decode a string that may contain C-style escapes into raw bytes.
/// Supports `\n` `\r` `\t` `\0` `\e` (ESC) `\xHH` `\\`.
pub fn unescape(input: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        let Some(esc) = chars.next() else {
            // Trailing backslash at end of input, emit it literally.
            out.push(b'\\');
            break;
        };
        match esc {
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            '0' => out.push(0),
            'e' | 'E' => out.push(0x1b),
            '\\' => out.push(b'\\'),
            'x' => {
                let h: String = (0..2).filter_map(|_| chars.next()).collect();
                if let Ok(b) = u8::from_str_radix(&h, 16) {
                    out.push(b);
                }
            }
            other => {
                out.push(b'\\');
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(shift: bool, alt: bool, ctrl: bool) -> Mods {
        Mods { shift, alt, ctrl }
    }

    #[test]
    fn plain_char_is_literal() {
        assert_eq!(key_to_bytes("a", Mods::default()), Some(b"a".to_vec()));
    }

    #[test]
    fn ctrl_c_is_control_code() {
        assert_eq!(
            key_to_bytes("c", mods(false, false, true)),
            Some(vec![0x03])
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(
            key_to_bytes("x", mods(false, true, false)),
            Some(vec![0x1b, b'x'])
        );
    }

    #[test]
    fn named_keys() {
        assert_eq!(key_to_bytes("enter", Mods::default()), Some(vec![b'\r']));
        assert_eq!(key_to_bytes("esc", Mods::default()), Some(vec![0x1b]));
        assert_eq!(
            key_to_bytes("up", Mods::default()),
            Some(vec![0x1b, b'[', b'A'])
        );
        assert_eq!(key_to_bytes("backspace", Mods::default()), Some(vec![0x7f]));
    }

    #[test]
    fn piped_enter_ends_a_line_and_cr_stays_cr() {
        let shift = Mods {
            shift: true,
            ..Mods::default()
        };
        for name in ["enter", "Return"] {
            assert_eq!(piped_key_to_bytes(name, Mods::default()), Some(vec![b'\n']));
            assert_eq!(piped_key_to_bytes(name, shift), Some(vec![b'\n']));
        }
        assert_eq!(piped_key_to_bytes("cr", Mods::default()), Some(vec![b'\r']));
        // Every other key is encoded as in a terminal.
        assert_eq!(piped_key_to_bytes("up", shift), key_to_bytes("up", shift));
        assert_eq!(piped_key_to_bytes("nope", shift), None);
    }

    #[test]
    fn arrow_with_modifier_uses_csi_param() {
        assert_eq!(
            key_to_bytes("up", mods(false, false, true)),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn unknown_multichar_key_is_none() {
        assert_eq!(key_to_bytes("notakey", Mods::default()), None);
    }

    #[test]
    fn kitty_disabled_returns_none() {
        assert_eq!(kitty_encode("c", mods(false, false, true), 0), None);
    }

    #[test]
    fn kitty_encodes_ctrl_c() {
        assert_eq!(
            kitty_encode("c", mods(false, false, true), 1),
            Some(b"\x1b[99;5u".to_vec())
        );
    }

    #[test]
    fn kitty_plain_key_falls_back_unless_report_all() {
        // Disambiguate mode: unmodified text keys stay literal (None -> legacy).
        assert_eq!(kitty_encode("a", Mods::default(), 1), None);
        // Report-all-keys mode (bit 3): even plain keys are encoded.
        assert_eq!(
            kitty_encode("a", Mods::default(), 0b1000),
            Some(b"\x1b[97u".to_vec())
        );
    }

    #[test]
    fn unescape_handles_c_escapes() {
        assert_eq!(unescape(r"\n\r\t"), vec![b'\n', b'\r', b'\t']);
        assert_eq!(unescape(r"\e"), vec![0x1b]);
        assert_eq!(unescape(r"\x1b[A"), vec![0x1b, b'[', b'A']);
        assert_eq!(unescape(r"\\"), vec![b'\\']);
        assert_eq!(unescape("plain"), b"plain".to_vec());
    }

    #[test]
    fn unescape_trailing_backslash_is_literal() {
        assert_eq!(unescape(r"ab\"), vec![b'a', b'b', b'\\']);
    }
}
