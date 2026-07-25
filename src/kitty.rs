//! Detection of terminal input-mode negotiation on a program's output stream:
//! the kitty keyboard protocol and bracketed paste mode.
//!
//! For the kitty protocol a TUI pushes progressive-enhancement flags with
//! `CSI > flags u`, sets them with `CSI = flags ; mode u`, pops with
//! `CSI < number u`, and queries the current flags with `CSI ? u`. We watch the
//! program's output for these, keep the current flag value in a shared atomic,
//! and answer the query so the program learns the protocol is supported. The
//! input side (`keys::kitty_encode`) then encodes key presses accordingly.
//!
//! Bracketed paste is enabled with `CSI ? 2004 h` and disabled with
//! `CSI ? 2004 l`; we track it so pasted text can be wrapped in the paste
//! markers only when the program actually understands them.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Shared current kitty keyboard flags. `0` means disabled (legacy encoding).
pub type KittyFlags = Arc<AtomicU8>;

/// Shared bracketed-paste-mode state (`true` = the program enabled it).
pub type PasteFlag = Arc<AtomicBool>;

/// Streaming scanner for terminal input-mode control sequences.
pub struct KittyDetector {
    flags: KittyFlags,
    paste: PasteFlag,
    buf: Vec<u8>,
    in_csi: bool,
    saw_esc: bool,
    /// A cursor-position report (CPR, `CSI 6 n`) was requested and not yet answered.
    cpr_requested: bool,
}

impl KittyDetector {
    pub fn new(flags: KittyFlags, paste: PasteFlag) -> Self {
        Self {
            flags,
            paste,
            buf: Vec::new(),
            in_csi: false,
            saw_esc: false,
            cpr_requested: false,
        }
    }

    /// Whether a cursor-position report (DSR `CSI 6 n`) was requested since the
    /// last check, clearing the flag. The caller answers with the emulator's
    /// current cursor because only it knows the position.
    ///
    /// This matters on Windows: ConPTY is created with
    /// `PSEUDOCONSOLE_INHERIT_CURSOR`, so conhost emits `CSI 6 n` at startup and
    /// blocks its whole initialisation until the driving terminal replies. Never
    /// answering it leaves every interactive program frozen with a blank screen.
    pub fn take_cursor_report_request(&mut self) -> bool {
        std::mem::take(&mut self.cpr_requested)
    }

    /// Feed a chunk of program output. Returns bytes that must be written back
    /// to the program (a response to a flags query), or empty.
    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        let mut reply = Vec::new();
        for &b in data {
            if self.in_csi {
                self.buf.push(b);
                // CSI ends at a final byte in 0x40..=0x7e.
                if (0x40..=0x7e).contains(&b) {
                    self.handle_csi(&mut reply);
                    self.in_csi = false;
                    self.buf.clear();
                } else if self.buf.len() > 32 {
                    // Runaway / not a real CSI we care about, so abort.
                    self.in_csi = false;
                    self.buf.clear();
                }
                continue;
            }
            if self.saw_esc {
                self.saw_esc = false;
                if b == b'[' {
                    self.in_csi = true;
                    self.buf.clear();
                }
                continue;
            }
            if b == 0x1b {
                self.saw_esc = true;
            }
        }
        reply
    }

    fn handle_csi(&mut self, reply: &mut Vec<u8>) {
        let Some((&final_b, body)) = self.buf.split_last() else {
            return;
        };
        // Bracketed paste: CSI ? 2004 h (enable) / CSI ? 2004 l (disable).
        if matches!(final_b, b'h' | b'l') {
            if body.first() == Some(&b'?') && body[1..].split(|&c| c == b';').any(|p| p == b"2004")
            {
                self.paste.store(final_b == b'h', Ordering::Relaxed);
            }
            return;
        }
        // Cursor-position report request: DSR `CSI 6 n` (or private `CSI ? 6 n`).
        // We only flag it here; the reader thread answers with the real cursor.
        if final_b == b'n' {
            if body == b"6" || body == b"?6" {
                self.cpr_requested = true;
            }
            return;
        }
        // Everything else we care about is the kitty 'u' terminator.
        if final_b != b'u' {
            return;
        }
        match body.first() {
            // Push flags: CSI > flags u  (flags default to 1 if omitted).
            Some(b'>') => {
                let n = parse_num(&body[1..]).unwrap_or(1);
                self.flags.store(n, Ordering::Relaxed);
            }
            // Set flags: CSI = flags ; mode u.
            Some(b'=') => {
                let first = body[1..].split(|&c| c == b';').next().unwrap_or(&[]);
                let n = parse_num(first).unwrap_or(0);
                self.flags.store(n, Ordering::Relaxed);
            }
            // Pop: CSI < number u, simplified to fully disable.
            Some(b'<') => {
                self.flags.store(0, Ordering::Relaxed);
            }
            // Query: CSI ? u, respond with the current flags.
            Some(b'?') => {
                let cur = self.flags.load(Ordering::Relaxed);
                reply.extend_from_slice(format!("\x1b[?{cur}u").as_bytes());
            }
            _ => {}
        }
    }
}

fn parse_num(b: &[u8]) -> Option<u8> {
    if b.is_empty() {
        return None;
    }
    std::str::from_utf8(b).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> (KittyDetector, KittyFlags, PasteFlag) {
        let flags: KittyFlags = Arc::new(AtomicU8::new(0));
        let paste: PasteFlag = Arc::new(AtomicBool::new(false));
        (
            KittyDetector::new(Arc::clone(&flags), Arc::clone(&paste)),
            flags,
            paste,
        )
    }

    #[test]
    fn push_sets_flags() {
        let (mut d, flags, _paste) = detector();
        let reply = d.feed(b"\x1b[>1u");
        assert!(reply.is_empty());
        assert_eq!(flags.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn set_flags_form() {
        let (mut d, flags, _paste) = detector();
        d.feed(b"\x1b[=5;1u");
        assert_eq!(flags.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn pop_disables() {
        let (mut d, flags, _paste) = detector();
        d.feed(b"\x1b[>7u");
        assert_eq!(flags.load(Ordering::Relaxed), 7);
        d.feed(b"\x1b[<u");
        assert_eq!(flags.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn query_is_answered_with_current_flags() {
        let (mut d, _flags, _paste) = detector();
        d.feed(b"\x1b[>2u");
        let reply = d.feed(b"\x1b[?u");
        assert_eq!(reply, b"\x1b[?2u");
    }

    #[test]
    fn sequence_split_across_feeds() {
        let (mut d, flags, _paste) = detector();
        d.feed(b"\x1b[>");
        d.feed(b"1u");
        assert_eq!(flags.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn ignores_unrelated_output() {
        let (mut d, flags, _paste) = detector();
        d.feed(b"hello \x1b[2J world \x1b[1;5H");
        assert_eq!(flags.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn cursor_position_report_is_flagged() {
        let (mut d, _flags, _paste) = detector();
        assert!(d.feed(b"\x1b[6n").is_empty());
        assert!(d.take_cursor_report_request());
        // The flag is one-shot: a second check sees nothing.
        assert!(!d.take_cursor_report_request());
    }

    #[test]
    fn cursor_position_report_split_across_feeds() {
        let (mut d, _flags, _paste) = detector();
        d.feed(b"\x1b[");
        d.feed(b"6");
        d.feed(b"n");
        assert!(d.take_cursor_report_request());
    }

    #[test]
    fn private_cursor_position_report_is_flagged() {
        // ConPTY may send the private form `CSI ? 6 n`.
        let (mut d, _flags, _paste) = detector();
        d.feed(b"\x1b[?6n");
        assert!(d.take_cursor_report_request());
    }

    #[test]
    fn unrelated_dsr_is_ignored() {
        let (mut d, _flags, _paste) = detector();
        // Device-attributes-ish `CSI 5 n` is not a cursor report.
        d.feed(b"\x1b[5n");
        assert!(!d.take_cursor_report_request());
    }

    #[test]
    fn bracketed_paste_toggles() {
        let (mut d, _flags, paste) = detector();
        d.feed(b"\x1b[?2004h");
        assert!(paste.load(Ordering::Relaxed));
        d.feed(b"\x1b[?2004l");
        assert!(!paste.load(Ordering::Relaxed));
    }

    #[test]
    fn other_private_modes_do_not_touch_paste() {
        let (mut d, _flags, paste) = detector();
        d.feed(b"\x1b[?1000h\x1b[?25l");
        assert!(!paste.load(Ordering::Relaxed));
    }
}
