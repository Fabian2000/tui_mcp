//! Screen sampling and reply formatting for the `wait_*` tools.
//!
//! The wait tools poll a pty screen in a loop. Each poll takes one snapshot
//! under a single session lock so the match predicate and the rendering that is
//! returned observe the *same* frame — there is no separate `read_screen` after
//! the match that could sample a later, different frame.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::session::{ScreenDump, ScreenFormat, Session, SessionManager};

/// How much of the screen a `wait_*` tool returns once it resolves.
#[derive(
    Serialize, /* Without it json schema won't be able to display "default" value */
    Deserialize,
    JsonSchema,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Debug,
)]
#[serde(rename_all = "lowercase")]
#[schemars(inline, transform = crate::schema::flatten_enum)]
pub(crate) enum WaitReturn {
    /// Return only the outcome (matched / timed out); no screen payload.
    None,
    /// Return the screen as plain text.
    #[default]
    Text,
    /// Return the screen with ANSI SGR escapes (colors / attributes).
    Ansi,
}

/// One poll of a pty screen taken under a single lock, so the predicate and the
/// returned rendering observe the *same* frame (no match-then-reread race).
pub(crate) struct ScreenSample {
    /// Plain-text render; the predicate always runs against this.
    pub(crate) base: ScreenDump,
    /// Whether the predicate held on this frame.
    pub(crate) matched: bool,
    /// ANSI render of the same frame — only taken when it will actually be
    /// returned (`format=ansi` and this is the sample we resolve on).
    ansi: Option<ScreenDump>,
}

/// Poll a pty session once and evaluate `pred` against its plain-text screen,
/// all while holding the session lock. `who` names the calling tool for the
/// piped-session error. When `format` is `Ansi`, an ANSI render of the same
/// frame is captured iff the predicate matched or `force_render` is set (the
/// latter covers the timeout path) — never on a poll we won't return from.
///
/// Returns an `anyhow::Error` on failure; the caller maps it to an MCP error.
pub(crate) fn sample_screen(
    sessions: &SessionManager,
    name: &str,
    who: &str,
    format: WaitReturn,
    force_render: bool,
    pred: impl FnOnce(&str) -> bool,
) -> anyhow::Result<ScreenSample> {
    sessions.with(name, |s| match s {
        Session::Pty(p) => {
            let base = p.dump(ScreenFormat::Text);
            let matched = pred(&base.text);
            let ansi = (format == WaitReturn::Ansi && (matched || force_render))
                .then(|| p.dump(ScreenFormat::Ansi));
            Ok(ScreenSample {
                base,
                matched,
                ansi,
            })
        }
        Session::Piped(_) => Err(anyhow::anyhow!("{who} needs a pty session")),
    })
}

/// Build a `wait_*` reply: the outcome word alone for `None`, or the outcome
/// followed by the rendered screen for `Text` / `Ansi`.
pub(crate) fn wait_reply(outcome: &str, format: WaitReturn, s: &ScreenSample) -> String {
    match format {
        WaitReturn::None => outcome.to_string(),
        WaitReturn::Text => format!("{outcome}\n{}", s.base.render()),
        // Falls back to the text frame if the ANSI render is somehow absent;
        // in practice `sample_screen` guarantees it on any returned sample.
        WaitReturn::Ansi => format!("{outcome}\n{}", s.ansi.as_ref().unwrap_or(&s.base).render()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump(text: &str) -> ScreenDump {
        ScreenDump {
            text: text.to_string(),
            cursor_row: 9,
            cursor_col: 0,
            cursor_hidden: false,
            rows: 40,
            cols: 120,
        }
    }

    #[test]
    fn wait_reply_none_is_outcome_only() {
        let s = ScreenSample {
            base: dump("hello world"),
            matched: true,
            ansi: None,
        };
        assert_eq!(wait_reply("matched", WaitReturn::None, &s), "matched");
    }

    #[test]
    fn wait_reply_text_appends_screen() {
        let s = ScreenSample {
            base: dump("hello"),
            matched: true,
            ansi: None,
        };
        let out = wait_reply("matched", WaitReturn::Text, &s);
        assert_eq!(out, format!("matched\n{}", dump("hello").render()));
        assert!(out.contains("hello"));
    }

    #[test]
    fn wait_reply_ansi_uses_ansi_frame_when_present() {
        let s = ScreenSample {
            base: dump("plain"),
            matched: true,
            ansi: Some(dump("ansi-frame")),
        };
        let out = wait_reply("matched", WaitReturn::Ansi, &s);
        assert!(out.contains("ansi-frame"));
        assert!(!out.contains("plain"));
    }

    #[test]
    fn wait_reply_ansi_falls_back_to_base_frame() {
        // Defensive: if the ANSI render is missing, render the text frame
        // rather than panicking.
        let s = ScreenSample {
            base: dump("plain"),
            matched: true,
            ansi: None,
        };
        let out = wait_reply("matched", WaitReturn::Ansi, &s);
        assert!(out.contains("plain"));
    }
}
