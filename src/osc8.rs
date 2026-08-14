//! Best-effort capture of OSC 8 hyperlinks from a terminal output stream.
//!
//! The `vt100` emulator does not model OSC 8 hyperlinks, so their URIs are lost
//! from the rendered screen. This tracker scans the raw byte stream for
//! `ESC ] 8 ; params ; URI ST` (open) and `ESC ] 8 ; ; ST` (close), records the
//! display text written between them, and keeps a bounded log of
//! `(display text, uri)` pairs. Positions are not tracked, so links are matched
//! back to the screen by display text at read time.

use std::sync::Arc;

use parking_lot::Mutex;

const MAX_LINKS: usize = 128;
const MAX_TEXT: usize = 256;
const MAX_URI: usize = 4096;
const MAX_OSC: usize = MAX_URI + 64;
const MAX_SHOWN: usize = 32;

/// Shared bounded log of captured `(display text, uri)` pairs.
pub type HyperlinkLog = Arc<Mutex<Vec<(String, String)>>>;

enum State {
    Normal,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

/// Streaming scanner that records OSC 8 hyperlinks into a [`HyperlinkLog`].
pub struct Osc8Tracker {
    log: HyperlinkLog,
    state: State,
    osc: Vec<u8>,
    current_uri: Option<String>,
    display: Vec<u8>,
}

impl Osc8Tracker {
    pub fn new(log: HyperlinkLog) -> Self {
        Self {
            log,
            state: State::Normal,
            osc: Vec::new(),
            current_uri: None,
            display: Vec::new(),
        }
    }

    /// Feed a chunk of program output.
    pub fn feed(&mut self, data: &[u8]) {
        for &b in data {
            match self.state {
                State::Normal => self.on_normal(b),
                State::Esc => self.on_esc(b),
                State::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = State::Normal;
                    }
                }
                State::Osc => self.on_osc(b),
                State::OscEsc => {
                    self.finish_osc();
                    self.state = State::Normal;
                    if b != b'\\' {
                        self.on_normal(b);
                    }
                }
            }
        }
    }

    fn on_normal(&mut self, b: u8) {
        if b == 0x1b {
            self.state = State::Esc;
        } else if self.current_uri.is_some()
            && b >= 0x20
            && b != 0x7f
            && self.display.len() < MAX_TEXT
        {
            self.display.push(b);
        }
    }

    fn on_esc(&mut self, b: u8) {
        match b {
            b'[' => self.state = State::Csi,
            b']' => {
                self.state = State::Osc;
                self.osc.clear();
            }
            _ => self.state = State::Normal,
        }
    }

    fn on_osc(&mut self, b: u8) {
        match b {
            0x07 => {
                self.finish_osc();
                self.state = State::Normal;
            }
            0x1b => self.state = State::OscEsc,
            _ => {
                if self.osc.len() < MAX_OSC {
                    self.osc.push(b);
                }
            }
        }
    }

    fn finish_osc(&mut self) {
        let body = std::mem::take(&mut self.osc);
        let Some(rest) = body.strip_prefix(b"8;") else {
            return;
        };
        let Some(sep) = rest.iter().position(|&c| c == b';') else {
            return;
        };
        let uri = &rest[sep + 1..];
        if uri.is_empty() {
            self.close_link();
        } else {
            self.close_link();
            let uri = &uri[..uri.len().min(MAX_URI)];
            self.current_uri = Some(String::from_utf8_lossy(uri).into_owned());
            self.display.clear();
        }
    }

    fn close_link(&mut self) {
        let Some(uri) = self.current_uri.take() else {
            return;
        };
        let text = String::from_utf8_lossy(&self.display).trim().to_string();
        self.display.clear();
        if text.is_empty() {
            return;
        }
        let mut log = self.log.lock();
        if log.len() >= MAX_LINKS {
            log.remove(0);
        }
        log.push((text, uri));
    }
}

/// Build a footnote listing links whose display text is present in
/// `plain_screen`, deduplicated and capped. Returns `None` if none match.
pub fn visible_footnote(plain_screen: &str, links: &[(String, String)]) -> Option<String> {
    let mut out = String::new();
    let mut shown = 0;
    for (text, uri) in links {
        if shown >= MAX_SHOWN {
            break;
        }
        if !plain_screen.contains(text.as_str()) {
            continue;
        }
        let entry = format!("  [{text}] -> {uri}\n");
        if out.contains(&entry) {
            continue;
        }
        out.push_str(&entry);
        shown += 1;
    }
    if out.is_empty() {
        return None;
    }
    let mut footnote = String::from("Hyperlinks:\n");
    footnote.push_str(&out);
    Some(footnote)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> (Osc8Tracker, HyperlinkLog) {
        let log: HyperlinkLog = Arc::new(Mutex::new(Vec::new()));
        (Osc8Tracker::new(Arc::clone(&log)), log)
    }

    fn osc8(uri: &str, text: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"\x1b]8;;");
        v.extend_from_slice(uri.as_bytes());
        v.extend_from_slice(b"\x1b\\");
        v.extend_from_slice(text.as_bytes());
        v.extend_from_slice(b"\x1b]8;;\x1b\\");
        v
    }

    #[test]
    fn captures_link_with_st_terminator() {
        let (mut t, log) = tracker();
        t.feed(&osc8("https://example.com", "EXAMPLE"));
        assert_eq!(
            *log.lock(),
            vec![("EXAMPLE".to_string(), "https://example.com".to_string())]
        );
    }

    #[test]
    fn captures_link_with_bel_terminator() {
        let (mut t, log) = tracker();
        t.feed(b"\x1b]8;;https://a.test\x07LINK\x1b]8;;\x07");
        assert_eq!(
            *log.lock(),
            vec![("LINK".to_string(), "https://a.test".to_string())]
        );
    }

    #[test]
    fn ignores_non_hyperlink_osc() {
        let (mut t, log) = tracker();
        t.feed(b"\x1b]0;window title\x1b\\plain");
        assert!(log.lock().is_empty());
    }

    #[test]
    fn strips_sgr_inside_link_text() {
        let (mut t, log) = tracker();
        t.feed(b"\x1b]8;;https://x.test\x1b\\\x1b[31mRED\x1b[0m\x1b]8;;\x1b\\");
        assert_eq!(
            *log.lock(),
            vec![("RED".to_string(), "https://x.test".to_string())]
        );
    }

    #[test]
    fn split_across_feeds() {
        let (mut t, log) = tracker();
        t.feed(b"\x1b]8;;https://x.test");
        t.feed(b"\x1b\\HI\x1b]8;;\x1b\\");
        assert_eq!(
            *log.lock(),
            vec![("HI".to_string(), "https://x.test".to_string())]
        );
    }

    #[test]
    fn footnote_only_lists_visible_links() {
        let links = vec![
            ("EXAMPLE".to_string(), "https://example.com".to_string()),
            ("GONE".to_string(), "https://gone.test".to_string()),
        ];
        let screen = "here is EXAMPLE on screen";
        let footnote = visible_footnote(screen, &links).expect("footnote");
        assert!(footnote.contains("[EXAMPLE] -> https://example.com"));
        assert!(!footnote.contains("GONE"));
    }

    #[test]
    fn footnote_none_when_no_visible_links() {
        let links = vec![("GONE".to_string(), "https://gone.test".to_string())];
        assert!(visible_footnote("nothing here", &links).is_none());
    }
}
