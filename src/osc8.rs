//! Best-effort capture of OSC 8 hyperlinks from a terminal output stream.
//!
//! The `vt100` emulator does not model OSC 8 hyperlinks, so their URIs are lost
//! from the rendered screen. This tracker scans the raw byte stream for
//! `ESC ] 8 ; params ; URI ST` (open) and `ESC ] 8 ; ; ST` (close), records the
//! display text written between them, and keeps a bounded log of
//! `(display text, uri)` pairs. Positions are not tracked, so links are matched
//! back to the screen by display text at read time, and an entry is retired once
//! no on-screen occurrence of its text is left unclaimed by a newer entry.

use std::collections::HashMap;
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

/// Build a footnote listing the logged links whose display text appears in `plain_screen`, at most
/// `MAX_SHOWN` of them; `None` if none are visible.
///
/// As a side effect, `links` is pruned: each display text keeps at most as many entries as it has
/// occurrences on screen, newest first — so a relinked label resolves to the URI drawn last, while
/// a table with one `view` link per row keeps every row. Pruning exists because matching is by
/// substring: a scrolled-away entry would otherwise resurface whenever later plain text repeats
/// its words, reporting a URI that is no longer on screen. Entries past `MAX_SHOWN` stay in the
/// log (still visible); only the listing is capped, and a trailing line says how many it hid.
pub fn visible_footnote(plain_screen: &str, links: &mut Vec<(String, String)>) -> Option<String> {
    // Keep at most as many entries per display text as that text has occurrences
    // on screen, newest first. A label the program relinked (same text, new URI)
    // occurs once and resolves to the URI drawn last; a table with a `view` link
    // per row occurs N times and keeps all N. Walking back to front is what makes
    // "newest" available, since the log is appended oldest-first.
    //
    // The budget is memoised per distinct text, so the screen is scanned once per
    // label rather than once per entry, and a TUI that relogs the same link every
    // frame exhausts the counter without scanning again.
    let mut budget: HashMap<String, usize> = HashMap::new();
    let mut kept: Vec<(String, String)> = links
        .drain(..)
        .rev()
        .filter(|(text, _)| {
            let slots = budget
                .entry(text.clone())
                .or_insert_with(|| plain_screen.matches(text.as_str()).count());
            if *slots == 0 {
                return false;
            }
            *slots -= 1;
            true
        })
        .collect();
    kept.reverse();
    *links = kept;

    if links.is_empty() {
        return None;
    }

    let mut footnote = String::from("Hyperlinks:\n");
    for (text, uri) in links.iter().take(MAX_SHOWN) {
        footnote.push_str(&format!("  [{text}] -> {uri}\n"));
    }
    let hidden = links.len().saturating_sub(MAX_SHOWN);
    if hidden > 0 {
        footnote.push_str(&format!("  ... {hidden} more not shown\n"));
    }
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

    fn link(text: &str, uri: &str) -> (String, String) {
        (text.to_string(), uri.to_string())
    }

    #[test]
    fn footnote_only_lists_visible_links() {
        let mut links = vec![
            link("EXAMPLE", "https://example.com"),
            link("GONE", "https://gone.test"),
        ];
        let screen = "here is EXAMPLE on screen";
        let footnote = visible_footnote(screen, &mut links).expect("footnote");
        assert!(footnote.contains("[EXAMPLE] -> https://example.com"));
        assert!(!footnote.contains("GONE"));
    }

    #[test]
    fn footnote_none_when_no_visible_links() {
        let mut links = vec![link("GONE", "https://gone.test")];
        assert!(visible_footnote("nothing here", &mut links).is_none());
    }

    #[test]
    fn scrolled_off_link_is_dropped_from_the_log() {
        let mut links = vec![
            link("EXAMPLE", "https://example.com"),
            link("GONE", "https://gone.test"),
        ];
        visible_footnote("here is EXAMPLE on screen", &mut links);
        assert_eq!(links, vec![link("EXAMPLE", "https://example.com")]);
    }

    #[test]
    fn dropped_link_does_not_resurface_as_plain_text() {
        let mut links = vec![link("GONE", "https://gone.test")];
        // Scrolled away: retired here...
        assert!(visible_footnote("nothing here", &mut links).is_none());
        // ...so a later line that merely mentions the words stays plain text.
        assert!(visible_footnote("a line about GONE things", &mut links).is_none());
        assert!(links.is_empty());
    }

    #[test]
    fn pruning_is_idempotent_for_visible_links() {
        let mut links = vec![link("EXAMPLE", "https://example.com")];
        let screen = "EXAMPLE stays put";
        let first = visible_footnote(screen, &mut links).expect("footnote");
        let second = visible_footnote(screen, &mut links).expect("footnote");
        assert_eq!(first, second);
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn duplicate_entries_collapse() {
        let mut links = vec![
            link("EXAMPLE", "https://example.com"),
            link("EXAMPLE", "https://example.com"),
        ];
        let footnote = visible_footnote("EXAMPLE", &mut links).expect("footnote");
        assert_eq!(
            footnote,
            "Hyperlinks:\n  [EXAMPLE] -> https://example.com\n"
        );
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn relinked_text_resolves_to_the_newest_uri() {
        let mut links = vec![
            link("docs", "https://old.test"),
            link("docs", "https://new.test"),
        ];
        // One occurrence on screen, so only the most recently logged URI can be
        // the live one.
        let footnote = visible_footnote("see docs", &mut links).expect("footnote");
        assert_eq!(footnote, "Hyperlinks:\n  [docs] -> https://new.test\n");
        assert_eq!(links, vec![link("docs", "https://new.test")]);
    }

    #[test]
    fn repeated_text_keeps_one_entry_per_occurrence() {
        let mut links = vec![
            link("view", "https://stale.test"),
            link("view", "https://row1.test"),
            link("view", "https://row2.test"),
        ];
        // Two rows on screen, three logged: the oldest is left over from a
        // previous frame.
        let footnote = visible_footnote("row1 view | row2 view", &mut links).expect("footnote");
        assert!(!footnote.contains("stale"));
        assert_eq!(
            links,
            vec![
                link("view", "https://row1.test"),
                link("view", "https://row2.test")
            ]
        );
    }

    #[test]
    fn budget_larger_than_the_log_keeps_everything() {
        let mut links = vec![link("view", "https://only.test")];
        // The word appears three times but only one of them is a link.
        visible_footnote("view view view", &mut links);
        assert_eq!(links, vec![link("view", "https://only.test")]);
    }

    #[test]
    fn surviving_entries_stay_in_oldest_first_order() {
        let mut links = vec![
            link("A", "https://a.test"),
            link("B", "https://b.test"),
            link("C", "https://c.test"),
        ];
        let footnote = visible_footnote("A B C", &mut links).expect("footnote");
        assert_eq!(
            footnote,
            "Hyperlinks:\n  [A] -> https://a.test\n  [B] -> https://b.test\n  [C] -> https://c.test\n"
        );
    }

    #[test]
    fn links_past_the_display_cap_are_still_kept() {
        let mut links: Vec<_> = (0..MAX_SHOWN + 5)
            .map(|i| link(&format!("L{i:02}"), &format!("https://n{i:02}.test")))
            .collect();
        let screen: String = links
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let footnote = visible_footnote(&screen, &mut links).expect("footnote");
        // Header, MAX_SHOWN entries, and the truncation marker.
        assert_eq!(footnote.lines().count(), MAX_SHOWN + 2);
        assert!(footnote.ends_with("  ... 5 more not shown\n"));
        assert_eq!(links.len(), MAX_SHOWN + 5);
    }

    #[test]
    fn no_truncation_marker_when_everything_fits() {
        let mut links: Vec<_> = (0..MAX_SHOWN)
            .map(|i| link(&format!("L{i:02}"), &format!("https://n{i:02}.test")))
            .collect();
        let screen: String = links
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let footnote = visible_footnote(&screen, &mut links).expect("footnote");
        assert_eq!(footnote.lines().count(), MAX_SHOWN + 1);
        assert!(!footnote.contains("not shown"));
    }
}
