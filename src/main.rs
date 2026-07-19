//! `tui_mcp`: an MCP server for remote-controlling and observing TUI programs.
//!
//! Start a program under a real PTY (full terminal emulation) or with plain
//! pipes, then drive it with simulated keyboard / mouse input and read back the
//! rendered screen as text.

mod keys;
mod kitty;
mod mouse;
mod render;
mod session;

use std::collections::HashMap;
use std::fmt::Write as _;

use base64::Engine as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use keys::{Mods, key_to_bytes, unescape};
use mouse::{action_to_bytes, parse_action};
use session::{ScreenFormat, Session, SessionManager, SpawnOpts, Stream};

// ---- Tool input schemas ----------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct StartArgs {
    /// Unique name to refer to this session in later calls.
    name: String,
    /// Program to run, e.g. "vim" or "htop".
    command: String,
    /// Arguments passed to the program.
    #[serde(default)]
    args: Vec<String>,
    /// Working directory.
    #[serde(default)]
    cwd: Option<String>,
    /// Extra environment variables.
    #[serde(default)]
    env: HashMap<String, String>,
    /// Terminal columns (pty mode). Default 80.
    #[serde(default)]
    cols: Option<u16>,
    /// Terminal rows (pty mode). Default 24.
    #[serde(default)]
    rows: Option<u16>,
    /// "pty" (default, real terminal for TUIs) or "piped" (separate
    /// stdout/stderr, no TTY).
    #[serde(default)]
    mode: Option<String>,
    /// Per-stream output buffer cap in bytes for piped mode (ring buffer,
    /// oldest bytes drop when exceeded). Default 2 MiB.
    #[serde(default)]
    buffer_bytes: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct NameArg {
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct SendKeyArgs {
    name: String,
    /// Key name: a single char, or "enter", "tab", "esc", "up", "f5",
    /// "pageup", "delete", etc.
    key: String,
    /// Modifiers: any of "ctrl", "alt", "shift".
    #[serde(default)]
    modifiers: Vec<String>,
    /// Repeat the key this many times (default 1).
    #[serde(default)]
    count: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct SendKeysArgs {
    name: String,
    /// A sequence of key presses sent in order. Each entry is a key name with
    /// optional modifiers joined by '+', e.g.
    /// `["ctrl+c", "enter", "h", "i", "up", "alt+shift+f"]`.
    keys: Vec<String>,
    /// Optional delay between key presses in milliseconds. Omit or 0 for none.
    #[serde(default)]
    delay_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct SendTextArgs {
    name: String,
    /// Literal text typed verbatim into the program.
    text: String,
    /// Optional delay between characters in milliseconds, to pace input for
    /// programs that drop fast bursts. Omit or 0 to send it all at once.
    #[serde(default)]
    delay_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct SignalArgs {
    name: String,
    /// Signal name, e.g. "INT", "TERM", "HUP", "KILL", "TSTP", "CONT",
    /// "WINCH", "USR1", "USR2" (a leading "SIG" is also accepted).
    signal: String,
}

#[derive(Deserialize, JsonSchema)]
struct PasteArgs {
    name: String,
    /// Text to paste. If the program enabled bracketed paste mode, it is wrapped
    /// in paste markers so editors treat it as a paste (no auto-indent / no
    /// command execution). Otherwise it is sent as-is.
    text: String,
}

#[derive(Deserialize, JsonSchema)]
struct SendBytesArgs {
    name: String,
    /// Bytes with C-style escapes: \n \r \t \0 \e (ESC) \xHH \\.
    data: String,
}

#[derive(Deserialize, JsonSchema)]
struct MouseArgs {
    name: String,
    /// 1-based column.
    x: u16,
    /// 1-based row.
    y: u16,
    /// One of: `left`, `right`, `middle`, `scroll_up`, `scroll_down`, `down`,
    /// `up`, `move`/`drag`, `hover`.
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    modifiers: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ScreenArgs {
    name: String,
    /// "text" (plain, default) or "ansi" (with color/attribute escapes).
    #[serde(default)]
    format: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ResizeArgs {
    name: String,
    cols: u16,
    rows: u16,
}

#[derive(Deserialize, JsonSchema)]
struct ReadStreamArgs {
    name: String,
    /// "stdout" (default) or "stderr".
    #[serde(default)]
    stream: Option<String>,
    /// If true, clear the buffer after reading (default false).
    #[serde(default)]
    clear: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct WaitTextArgs {
    name: String,
    /// Text to wait for on screen (substring, or regex if `regex` is true).
    text: String,
    /// Treat `text` as a regular expression instead of a literal substring.
    #[serde(default)]
    regex: Option<bool>,
    /// Wait for the text to be ABSENT (disappear) instead of present.
    #[serde(default)]
    absent: Option<bool>,
    /// Max time to wait in milliseconds (default 5000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct WaitChangeArgs {
    name: String,
    /// Max time to wait in milliseconds (default 5000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct WaitExitArgs {
    name: String,
    /// Max time to wait in milliseconds (default 5000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct WaitOutputArgs {
    name: String,
    /// Text to wait for (substring, or regex if `regex` is true).
    text: String,
    /// Which piped stream to watch: "stdout" (default) or "stderr".
    #[serde(default)]
    stream: Option<String>,
    /// Treat `text` as a regular expression.
    #[serde(default)]
    regex: Option<bool>,
    /// Max time to wait in milliseconds (default 5000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct WaitStableArgs {
    name: String,
    /// Screen must be unchanged for this many ms (default 300).
    #[serde(default)]
    stable_ms: Option<u64>,
    /// Give up after this many ms (default 5000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct ScrollbackArgs {
    name: String,
    /// Lines per page (default 100).
    #[serde(default)]
    page_size: Option<usize>,
    /// 0-based page index. Omit to get the most recent page (the tail).
    #[serde(default)]
    page: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct SearchArgs {
    name: String,
    /// Regular expression to match against each scrollback line.
    pattern: String,
    /// Case-insensitive matching (default false).
    #[serde(default)]
    ignore_case: Option<bool>,
    /// Max matching lines to return (default 50).
    #[serde(default)]
    max_results: Option<usize>,
    /// Lines of context to include before and after each match (default 0).
    #[serde(default)]
    context: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct SleepArgs {
    /// How long to sleep, in milliseconds. Capped at `max_ms`.
    ms: u64,
    /// Upper bound on the sleep, in milliseconds (default and hard cap: 60000).
    #[serde(default)]
    max_ms: Option<u64>,
}

// ---- Server ----------------------------------------------------------------

#[derive(Clone)]
struct TuiServer {
    sessions: SessionManager,
    tool_router: ToolRouter<Self>,
}

fn reply(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

/// Parse a key combo like "ctrl+shift+a" or "enter" into `(key, modifiers)`.
/// The last '+'-separated token is the key, and earlier tokens are modifiers.
fn parse_combo(combo: &str) -> (String, Mods) {
    let parts: Vec<&str> = combo.split('+').collect();
    // Empty last token means a literal '+' (e.g. "+" or "ctrl+"), so treat the
    // whole string as the key rather than producing an empty key name.
    if parts.len() <= 1 || parts.last().is_some_and(|s| s.is_empty()) {
        return (combo.to_string(), Mods::default());
    }
    let key = (*parts.last().unwrap()).to_string();
    let mods: Vec<String> = parts[..parts.len() - 1]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    (key, Mods::from_list(&mods))
}

/// Resolve a key + modifiers to bytes for a session, using kitty encoding when
/// the (pty) session has the protocol enabled, else the legacy encoding.
fn key_bytes_for(session: &session::Session, key: &str, mods: Mods) -> Option<Vec<u8>> {
    let legacy = key_to_bytes(key, mods)?;
    Some(match session {
        Session::Pty(p) => keys::kitty_encode(key, mods, p.kitty_flags()).unwrap_or(legacy),
        Session::Piped(_) => legacy,
    })
}

/// Map a signal name (with or without a leading "SIG") to a nix signal.
#[cfg(unix)]
fn parse_signal(name: &str) -> Option<nix::sys::signal::Signal> {
    use nix::sys::signal::Signal;
    let n = name.trim().to_ascii_uppercase();
    let n = n.strip_prefix("SIG").unwrap_or(&n);
    Some(match n {
        "INT" => Signal::SIGINT,
        "TERM" => Signal::SIGTERM,
        "KILL" => Signal::SIGKILL,
        "HUP" => Signal::SIGHUP,
        "QUIT" => Signal::SIGQUIT,
        "TSTP" => Signal::SIGTSTP,
        "CONT" => Signal::SIGCONT,
        "STOP" => Signal::SIGSTOP,
        "WINCH" => Signal::SIGWINCH,
        "USR1" => Signal::SIGUSR1,
        "USR2" => Signal::SIGUSR2,
        _ => return None,
    })
}

/// Quote a path for a shell command line if it contains awkward characters.
fn shell_quote(s: &str) -> String {
    if s.is_empty()
        || s.chars()
            .any(|c| c.is_whitespace() || "\"'\\$`".contains(c))
    {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

fn err(e: &anyhow::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn screen_format(s: Option<&str>) -> ScreenFormat {
    match s {
        Some("ansi") => ScreenFormat::Ansi,
        _ => ScreenFormat::Text,
    }
}

/// Format a screen dump as a human-readable block with a status line.
/// Size is width x height. Cursor is x (column) and y (row), both 0-based.
fn render_dump(d: &session::ScreenDump) -> String {
    format!(
        "[size {}W x {}H] cursor=(x{}, y{}){}\n{}",
        d.cols,
        d.rows,
        d.cursor_col,
        d.cursor_row,
        if d.cursor_hidden { " hidden" } else { "" },
        d.text
    )
}

#[tool_router]
impl TuiServer {
    fn new() -> Self {
        Self {
            sessions: SessionManager::new(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Start a program in a new session. Use mode 'pty' (default) \
        for interactive TUI programs, or 'piped' for tools whose stdout and stderr \
        should be read separately (no TTY)."
    )]
    async fn session_start(
        &self,
        Parameters(a): Parameters<StartArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mode_pty = a.mode.as_deref() != Some("piped");
        let opts = SpawnOpts {
            command: a.command,
            args: a.args,
            cwd: a.cwd,
            env: a.env,
            cols: a.cols.unwrap_or(80),
            rows: a.rows.unwrap_or(24),
            buffer_bytes: a.buffer_bytes.unwrap_or(2 * 1024 * 1024),
        };
        let (cmdline, cwd) = self
            .sessions
            .create(a.name.clone(), mode_pty, &opts)
            .map_err(|e| err(&e))?;
        let mode = if mode_pty { "pty" } else { "piped" };
        Ok(reply(format!(
            "started session '{}' ({mode} mode)\n  command: {cmdline}\n  cwd: {cwd}\n\
             \n  to reproduce: (cd {} && {cmdline})",
            a.name,
            shell_quote(&cwd),
        )))
    }

    #[tool(
        description = "List all active sessions with their kind, pid, command, cwd and exit status."
    )]
    async fn session_list(&self) -> Result<CallToolResult, McpError> {
        let list = self.sessions.list();
        if list.is_empty() {
            return Ok(reply("no active sessions"));
        }
        let mut out = String::new();
        for s in list {
            let st = s.status.unwrap_or_else(|| "running".to_string());
            let pid = s.pid.map_or_else(|| "-".to_string(), |p| p.to_string());
            let _ = writeln!(
                out,
                "{} [{}, pid {pid}, {st}]\n  command: {}\n  cwd: {}",
                s.name, s.kind, s.cmdline, s.cwd
            );
        }
        Ok(reply(out))
    }

    #[tool(
        description = "Send an OS signal to a session's process (Unix only). Signal names: \
        INT, TERM, KILL, HUP, QUIT, TSTP, CONT, STOP, WINCH, USR1, USR2 (leading 'SIG' \
        optional). Useful for testing signal handlers, graceful shutdown (TERM), config \
        reload (HUP), or suspend/resume (TSTP/CONT). The session is not removed."
    )]
    async fn signal(
        &self,
        Parameters(a): Parameters<SignalArgs>,
    ) -> Result<CallToolResult, McpError> {
        #[cfg(unix)]
        {
            let sig = parse_signal(&a.signal).ok_or_else(|| {
                McpError::invalid_params(format!("unknown signal '{}'", a.signal), None)
            })?;
            let pid = self.sessions.signal(&a.name, sig).map_err(|e| err(&e))?;
            Ok(reply(format!("sent {sig:?} to pid {pid}")))
        }
        #[cfg(not(unix))]
        {
            let _ = (&a.name, &a.signal);
            Err(McpError::invalid_params(
                "signals are only supported on Unix platforms".to_string(),
                None,
            ))
        }
    }

    #[tool(description = "Stop and remove a session, killing its program.")]
    async fn session_stop(
        &self,
        Parameters(a): Parameters<NameArg>,
    ) -> Result<CallToolResult, McpError> {
        self.sessions.remove(&a.name).map_err(|e| err(&e))?;
        Ok(reply(format!("stopped session '{}'", a.name)))
    }

    #[tool(
        description = "Remove all sessions whose program has already exited, freeing them. \
        Returns the names that were purged. Running sessions are left untouched."
    )]
    async fn session_purge(&self) -> Result<CallToolResult, McpError> {
        let purged = self.sessions.purge_exited();
        if purged.is_empty() {
            return Ok(reply("no exited sessions to purge"));
        }
        Ok(reply(format!(
            "purged {}: {}",
            purged.len(),
            purged.join(", ")
        )))
    }

    #[tool(
        description = "Press a named key (optionally with ctrl/alt/shift modifiers), \
        e.g. key='enter', key='c' modifiers=['ctrl'], key='up'. If the program has enabled \
        the kitty keyboard protocol, keys are encoded accordingly automatically."
    )]
    async fn send_key(
        &self,
        Parameters(a): Parameters<SendKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mods = Mods::from_list(&a.modifiers);
        // Validate the key name up front for a clean error, using the legacy
        // encoding as the fallback byte sequence.
        let legacy = key_to_bytes(&a.key, mods)
            .ok_or_else(|| McpError::invalid_params(format!("unknown key '{}'", a.key), None))?;
        let count = a.count.unwrap_or(1).max(1);
        let mut kitty_used = false;
        self.sessions
            .with(&a.name, |s| {
                let bytes = match s {
                    Session::Pty(p) => match keys::kitty_encode(&a.key, mods, p.kitty_flags()) {
                        Some(b) => {
                            kitty_used = true;
                            b
                        }
                        None => legacy.clone(),
                    },
                    Session::Piped(_) => legacy.clone(),
                };
                for _ in 0..count {
                    write_session(s, &bytes)?;
                }
                Ok(())
            })
            .map_err(|e| err(&e))?;
        Ok(reply(format!(
            "sent key '{}' x{}{}",
            a.key,
            count,
            if kitty_used { " (kitty)" } else { "" }
        )))
    }

    #[tool(description = "Send a sequence of key presses in one call, e.g. \
        keys=[\"ctrl+c\", \"enter\"] or keys=[\"h\",\"i\",\"enter\"]. Each entry is a key \
        name with optional ctrl/alt/shift modifiers joined by '+'. Saves round-trips \
        versus calling send_key repeatedly. (To type a literal '+', use send_text.)")]
    async fn send_keys(
        &self,
        Parameters(a): Parameters<SendKeysArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Validate every combo up front so a bad key aborts before sending any.
        let parsed: Vec<(String, Mods)> = a.keys.iter().map(|c| parse_combo(c)).collect();
        for (key, mods) in &parsed {
            if key_to_bytes(key, *mods).is_none() {
                return Err(McpError::invalid_params(
                    format!("unknown key '{key}'"),
                    None,
                ));
            }
        }
        let delay = a.delay_ms.unwrap_or(0);
        if delay > 0 {
            for (key, mods) in &parsed {
                self.sessions
                    .with(&a.name, |s| {
                        if let Some(bytes) = key_bytes_for(s, key, *mods) {
                            write_session(s, &bytes)?;
                        }
                        Ok(())
                    })
                    .map_err(|e| err(&e))?;
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        } else {
            self.sessions
                .with(&a.name, |s| {
                    for (key, mods) in &parsed {
                        if let Some(bytes) = key_bytes_for(s, key, *mods) {
                            write_session(s, &bytes)?;
                        }
                    }
                    Ok(())
                })
                .map_err(|e| err(&e))?;
        }
        Ok(reply(format!(
            "sent {} key(s): {}",
            parsed.len(),
            a.keys.join(" ")
        )))
    }

    #[tool(
        description = "Type literal text into the program (no special-key handling). Set \
        delay_ms to pace the characters for programs that drop fast input."
    )]
    async fn send_text(
        &self,
        Parameters(a): Parameters<SendTextArgs>,
    ) -> Result<CallToolResult, McpError> {
        match a.delay_ms {
            Some(delay) if delay > 0 => {
                for ch in a.text.chars() {
                    let mut buf = [0u8; 4];
                    let bytes = ch.encode_utf8(&mut buf).as_bytes().to_vec();
                    self.sessions
                        .with(&a.name, |s| write_session(s, &bytes))
                        .map_err(|e| err(&e))?;
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
            _ => {
                let bytes = a.text.into_bytes();
                self.sessions
                    .with(&a.name, |s| write_session(s, &bytes))
                    .map_err(|e| err(&e))?;
            }
        }
        Ok(reply("text sent"))
    }

    #[tool(
        description = "Paste multi-line text into the program. If the program has enabled \
        bracketed paste mode (most editors do while editing), the text is wrapped in paste \
        markers so it is treated as a single paste, so no auto-indent and no accidental command \
        execution. Otherwise it falls back to sending the text as-is. Prefer this over \
        send_text for pasting code/blocks into editors like vim or nano."
    )]
    async fn paste(
        &self,
        Parameters(a): Parameters<PasteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut bracketed = false;
        self.sessions
            .with(&a.name, |s| {
                let payload = match s {
                    Session::Pty(p) if p.paste_enabled() => {
                        bracketed = true;
                        format!("\x1b[200~{}\x1b[201~", a.text).into_bytes()
                    }
                    _ => a.text.clone().into_bytes(),
                };
                write_session(s, &payload)
            })
            .map_err(|e| err(&e))?;
        Ok(reply(if bracketed {
            "pasted (bracketed)"
        } else {
            "pasted (raw, program has not enabled bracketed paste)"
        }))
    }

    #[tool(
        description = "Send raw bytes with C-style escapes (\\n \\r \\t \\e \\xHH). \
        Use for escape sequences your key/mouse tools don't cover."
    )]
    async fn send_bytes(
        &self,
        Parameters(a): Parameters<SendBytesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let bytes = unescape(&a.data);
        let n = bytes.len();
        self.sessions
            .with(&a.name, |s| write_session(s, &bytes))
            .map_err(|e| err(&e))?;
        Ok(reply(format!("sent {n} bytes")))
    }

    #[tool(
        description = "Simulate a mouse event at 1-based (x, y). Actions: left, right, \
        middle, scroll_up, scroll_down, down, up, move/drag, hover. The program must \
        have mouse reporting enabled (most full-screen TUIs do)."
    )]
    async fn send_mouse(
        &self,
        Parameters(a): Parameters<MouseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let action_name = a.action.as_deref().unwrap_or("left");
        let action = parse_action(action_name).ok_or_else(|| {
            McpError::invalid_params(format!("unknown mouse action '{action_name}'"), None)
        })?;
        let mods = Mods::from_list(&a.modifiers);
        let seqs = action_to_bytes(action, a.x, a.y, mods);
        self.sessions
            .with(&a.name, |s| {
                for seq in &seqs {
                    write_session(s, seq)?;
                }
                Ok(())
            })
            .map_err(|e| err(&e))?;
        Ok(reply(format!("mouse '{action_name}' at ({},{})", a.x, a.y)))
    }

    #[tool(
        description = "Read the current terminal screen as text (pty sessions only). \
        format='text' (default) or 'ansi' for colors/attributes. Includes a status \
        line with size and cursor position."
    )]
    async fn read_screen(
        &self,
        Parameters(a): Parameters<ScreenArgs>,
    ) -> Result<CallToolResult, McpError> {
        let fmt = screen_format(a.format.as_deref());
        let out = self
            .sessions
            .with(&a.name, |s| match s {
                Session::Pty(p) => Ok(render_dump(&p.dump(fmt))),
                Session::Piped(_) => {
                    Err(anyhow::anyhow!("session is piped, use read_output instead"))
                }
            })
            .map_err(|e| err(&e))?;
        Ok(reply(out))
    }

    #[tool(
        description = "Take a PNG screenshot of the pty screen, for checking colors / \
        layout. PREFER read_screen (plain text) when you only need the content, since it is \
        much cheaper in tokens. Use this only when color or visual layout matters."
    )]
    async fn screenshot(
        &self,
        Parameters(a): Parameters<NameArg>,
    ) -> Result<CallToolResult, McpError> {
        let png = self
            .sessions
            .with(&a.name, |s| match s {
                Session::Pty(p) => p.screenshot(),
                Session::Piped(_) => Err(anyhow::anyhow!("screenshots need a pty session")),
            })
            .map_err(|e| err(&e))?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        Ok(CallToolResult::success(vec![Content::image(
            b64,
            "image/png".to_string(),
        )]))
    }

    #[tool(
        description = "Read the terminal scrollback history (visible screen + lines that \
        scrolled off the top), one page at a time so it doesn't flood context. Returns the \
        requested page with line numbers; omit `page` to get the most recent page. For \
        full-screen TUIs there is usually no scrollback, so use read_screen instead."
    )]
    async fn read_scrollback(
        &self,
        Parameters(a): Parameters<ScrollbackArgs>,
    ) -> Result<CallToolResult, McpError> {
        let lines = self
            .sessions
            .with(&a.name, |s| match s {
                Session::Pty(p) => Ok(p.scrollback_lines()),
                Session::Piped(_) => Err(anyhow::anyhow!("scrollback needs a pty session")),
            })
            .map_err(|e| err(&e))?;

        let total = lines.len();
        let page_size = a.page_size.unwrap_or(100).max(1);
        let total_pages = total.div_ceil(page_size).max(1);
        let page = a
            .page
            .unwrap_or(total_pages.saturating_sub(1))
            .min(total_pages - 1);
        let start = page * page_size;
        let end = (start + page_size).min(total);

        let mut out = format!(
            "scrollback: {total} lines, page {}/{} (lines {}-{})\n",
            page + 1,
            total_pages,
            start + 1,
            end
        );
        for (i, line) in lines[start..end].iter().enumerate() {
            let _ = writeln!(out, "{:>6} | {line}", start + i + 1);
        }
        Ok(reply(out))
    }

    #[tool(
        description = "Search the scrollback history with a regular expression. Returns \
        matching lines with their line numbers (and optional surrounding context), so you \
        can locate output without dumping the whole buffer."
    )]
    async fn search_scrollback(
        &self,
        Parameters(a): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let re = regex::RegexBuilder::new(&a.pattern)
            .case_insensitive(a.ignore_case.unwrap_or(false))
            .build()
            .map_err(|e| McpError::invalid_params(format!("bad regex: {e}"), None))?;
        let max = a.max_results.unwrap_or(50).max(1);
        let ctx = a.context.unwrap_or(0);

        let lines = self
            .sessions
            .with(&a.name, |s| match s {
                Session::Pty(p) => Ok(p.scrollback_lines()),
                Session::Piped(_) => Err(anyhow::anyhow!("scrollback needs a pty session")),
            })
            .map_err(|e| err(&e))?;

        let mut out = String::new();
        let mut hits = 0;
        for (i, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                if hits >= max {
                    let _ = writeln!(out, "... (more than {max} matches, truncated)");
                    break;
                }
                let lo = i.saturating_sub(ctx);
                let hi = (i + ctx + 1).min(lines.len());
                for (offset, line) in lines[lo..hi].iter().enumerate() {
                    let j = lo + offset;
                    let marker = if j == i { ">" } else { " " };
                    let _ = writeln!(out, "{marker}{:>6} | {line}", j + 1);
                }
                if ctx > 0 {
                    out.push_str("--\n");
                }
                hits += 1;
            }
        }
        if hits == 0 {
            return Ok(reply(format!(
                "no matches for /{}/ in {} lines",
                a.pattern,
                lines.len()
            )));
        }
        Ok(reply(format!("{hits} match(es):\n{out}")))
    }

    #[tool(description = "Resize the terminal of a pty session (cols x rows).")]
    async fn resize(
        &self,
        Parameters(a): Parameters<ResizeArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.sessions
            .with(&a.name, |s| match s {
                Session::Pty(p) => p.resize(a.cols, a.rows),
                Session::Piped(_) => Err(anyhow::anyhow!("cannot resize a piped session")),
            })
            .map_err(|e| err(&e))?;
        Ok(reply(format!("resized to {}x{}", a.cols, a.rows)))
    }

    #[tool(description = "Read buffered stdout or stderr of a piped session. \
        stream='stdout' (default) or 'stderr'. clear=true drains the buffer.")]
    async fn read_output(
        &self,
        Parameters(a): Parameters<ReadStreamArgs>,
    ) -> Result<CallToolResult, McpError> {
        let which = match a.stream.as_deref() {
            Some("stderr") => Stream::Stderr,
            _ => Stream::Stdout,
        };
        let clear = a.clear.unwrap_or(false);
        let out = self
            .sessions
            .with(&a.name, |s| match s {
                Session::Piped(p) => Ok(p.read_stream(which, clear)),
                Session::Pty(_) => Err(anyhow::anyhow!("session is pty, use read_screen instead")),
            })
            .map_err(|e| err(&e))?;
        Ok(reply(out))
    }

    #[tool(description = "Close the stdin of a piped session (signals EOF to the program).")]
    async fn close_stdin(
        &self,
        Parameters(a): Parameters<NameArg>,
    ) -> Result<CallToolResult, McpError> {
        self.sessions
            .with(&a.name, |s| match s {
                Session::Piped(p) => {
                    p.close_stdin();
                    Ok(())
                }
                Session::Pty(_) => Err(anyhow::anyhow!("not a piped session")),
            })
            .map_err(|e| err(&e))?;
        Ok(reply("stdin closed"))
    }

    #[tool(
        description = "Sleep for `ms` milliseconds (capped by `max_ms`, hard cap 60000). \
        Use to wait a fixed time for a program to react before reading the screen."
    )]
    async fn sleep(
        &self,
        Parameters(a): Parameters<SleepArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cap = a.max_ms.unwrap_or(60_000).min(60_000);
        let ms = a.ms.min(cap);
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        Ok(reply(format!("slept {ms}ms")))
    }

    #[tool(
        description = "Block until text appears on the screen (pty sessions), then return \
        the screen. Set regex=true to match a pattern, or absent=true to instead wait for \
        the text to DISAPPEAR (e.g. a 'Loading...' spinner). Use this to synchronize with \
        the program instead of guessing delays."
    )]
    async fn wait_for_text(
        &self,
        Parameters(a): Parameters<WaitTextArgs>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = a.timeout_ms.unwrap_or(5000);
        let absent = a.absent.unwrap_or(false);
        let re = if a.regex.unwrap_or(false) {
            Some(
                regex::Regex::new(&a.text)
                    .map_err(|e| McpError::invalid_params(format!("bad regex: {e}"), None))?,
            )
        } else {
            None
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout);
        loop {
            let dump = self
                .sessions
                .with(&a.name, |s| match s {
                    Session::Pty(p) => Ok(p.dump(ScreenFormat::Text)),
                    Session::Piped(_) => Err(anyhow::anyhow!("wait_for_text needs a pty session")),
                })
                .map_err(|e| err(&e))?;
            let found = match &re {
                Some(r) => r.is_match(&dump.text),
                None => dump.text.contains(&a.text),
            };
            if found != absent {
                let what = if absent { "gone" } else { "matched" };
                return Ok(reply(format!("{what}\n{}", render_dump(&dump))));
            }
            if tokio::time::Instant::now() >= deadline {
                let what = if absent { "still present" } else { "not found" };
                return Ok(reply(format!(
                    "TIMEOUT after {timeout}ms: text {what}\n{}",
                    render_dump(&dump)
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tool(
        description = "Block until the screen changes from its current contents, or timeout \
        (pty sessions). Useful after sending input when you expect any visible reaction."
    )]
    async fn wait_for_change(
        &self,
        Parameters(a): Parameters<WaitChangeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = a.timeout_ms.unwrap_or(5000);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout);
        let read = |srv: &Self| {
            srv.sessions.with(&a.name, |s| match s {
                Session::Pty(p) => Ok(p.dump(ScreenFormat::Text)),
                Session::Piped(_) => Err(anyhow::anyhow!("wait_for_change needs a pty session")),
            })
        };
        let initial = read(self).map_err(|e| err(&e))?.text;
        loop {
            let dump = read(self).map_err(|e| err(&e))?;
            if dump.text != initial {
                return Ok(reply(format!("changed\n{}", render_dump(&dump))));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(reply(format!(
                    "TIMEOUT after {timeout}ms: screen unchanged"
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tool(
        description = "Block until the program in a session exits, or timeout (both pty and \
        piped). Returns the exit status, or reports still-running on timeout."
    )]
    async fn wait_for_exit(
        &self,
        Parameters(a): Parameters<WaitExitArgs>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = a.timeout_ms.unwrap_or(5000);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout);
        loop {
            let status = self
                .sessions
                .with(&a.name, |s| {
                    Ok(match s {
                        Session::Pty(p) => p.exit_status(),
                        Session::Piped(p) => p.exit_status(),
                    })
                })
                .map_err(|e| err(&e))?;
            if let Some(st) = status {
                return Ok(reply(format!("exited: {st}")));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(reply(format!("TIMEOUT after {timeout}ms: still running")));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tool(
        description = "Block until the buffered stdout/stderr of a piped session contains \
        the given text (or regex), or timeout. The pty equivalent is wait_for_text."
    )]
    async fn wait_for_output(
        &self,
        Parameters(a): Parameters<WaitOutputArgs>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = a.timeout_ms.unwrap_or(5000);
        let which = match a.stream.as_deref() {
            Some("stderr") => Stream::Stderr,
            _ => Stream::Stdout,
        };
        let re = if a.regex.unwrap_or(false) {
            Some(
                regex::Regex::new(&a.text)
                    .map_err(|e| McpError::invalid_params(format!("bad regex: {e}"), None))?,
            )
        } else {
            None
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout);
        loop {
            let buf = self
                .sessions
                .with(&a.name, |s| match s {
                    Session::Piped(p) => Ok(p.read_stream(which, false)),
                    Session::Pty(_) => {
                        Err(anyhow::anyhow!("wait_for_output needs a piped session"))
                    }
                })
                .map_err(|e| err(&e))?;
            let found = match &re {
                Some(r) => r.is_match(&buf),
                None => buf.contains(&a.text),
            };
            if found {
                return Ok(reply(format!("matched\n{buf}")));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(reply(format!(
                    "TIMEOUT after {timeout}ms: not found\n{buf}"
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tool(
        description = "Block until the screen stops changing for `stable_ms`, or until \
        timeout (pty sessions). Useful to wait for a program to finish redrawing."
    )]
    async fn wait_for_stable(
        &self,
        Parameters(a): Parameters<WaitStableArgs>,
    ) -> Result<CallToolResult, McpError> {
        let stable_ms = a.stable_ms.unwrap_or(300);
        let timeout = a.timeout_ms.unwrap_or(5000);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout);
        let mut last = String::new();
        let mut stable_since = tokio::time::Instant::now();
        loop {
            let dump = self
                .sessions
                .with(&a.name, |s| match s {
                    Session::Pty(p) => Ok(p.dump(ScreenFormat::Text)),
                    Session::Piped(_) => {
                        Err(anyhow::anyhow!("wait_for_stable needs a pty session"))
                    }
                })
                .map_err(|e| err(&e))?;
            let now = tokio::time::Instant::now();
            if dump.text != last {
                last = dump.text.clone();
                stable_since = now;
            } else if now.duration_since(stable_since)
                >= std::time::Duration::from_millis(stable_ms)
            {
                return Ok(reply(format!("stable\n{}", render_dump(&dump))));
            }
            if now >= deadline {
                return Ok(reply(format!(
                    "TIMEOUT after {timeout}ms: still changing\n{}",
                    render_dump(&dump)
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

/// Write bytes to whichever session variant.
fn write_session(s: &mut Session, bytes: &[u8]) -> anyhow::Result<()> {
    match s {
        Session::Pty(p) => p.write(bytes),
        Session::Piped(p) => p.write(bytes),
    }
}

#[tool_handler]
impl rmcp::ServerHandler for TuiServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Remote-control TUI programs. Start a session (pty for TUIs, piped for \
                 line tools), then send_key / send_text / send_mouse, and read_screen \
                 (pty) or read_output (piped). Use wait_for_text / wait_for_stable to \
                 synchronize before reading."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// `$name $version`, e.g. `tui_mcp 1.1.0`. Sourced from Cargo so it always
/// matches the crate metadata (and the `serverInfo` the server reports).
fn version_line() -> String {
    format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}

fn help_text() -> String {
    format!(
        "{name} {version}\n\
         {desc}\n\n\
         USAGE:\n    {name} [OPTIONS]\n\n\
         OPTIONS:\n    \
         -h, --help       Print this help and exit\n    \
         -V, --version    Print version and exit\n\n\
         With no options, runs the MCP server over stdio: JSON-RPC on stdin/stdout,\n\
         logs on stderr.\n\
         Repository: {repo}",
        name = env!("CARGO_PKG_NAME"),
        version = env!("CARGO_PKG_VERSION"),
        desc = env!("CARGO_PKG_DESCRIPTION"),
        repo = env!("CARGO_PKG_REPOSITORY"),
    )
}

/// What the top-level CLI arguments ask the process to do. Every variant is a
/// normal (exit-0) outcome; a usage error is the `Err` side of the parse result.
#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    /// No recognized flag: start the MCP server (the default).
    RunServer,
    /// Print the version line to stdout, then exit.
    ShowVersion,
    /// Print the help text to stdout, then exit.
    ShowHelp,
}

/// Decide what to do from the argv tail (everything after the program name).
/// Only the first argument is significant. Returns the action to take, or an
/// `Err` holding a ready-to-print usage-error message. Kept pure (no I/O, no
/// exit) so it can be unit-tested.
fn parse_cli_args<I, S>(args: I) -> Result<CliAction, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    match args.into_iter().next() {
        None => Ok(CliAction::RunServer),
        Some(arg) => match arg.as_ref() {
            "--version" | "-V" => Ok(CliAction::ShowVersion),
            "--help" | "-h" => Ok(CliAction::ShowHelp),
            other => Err(format!(
                "unknown argument: {other}\nTry '{} --help'.",
                env!("CARGO_PKG_NAME")
            )),
        },
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Handle CLI flags before touching stdio, so `--version`/`--help` print and
    // exit instead of blocking in the JSON-RPC loop waiting for a client.
    match parse_cli_args(std::env::args().skip(1)) {
        Ok(CliAction::ShowVersion) => {
            println!("{}", version_line());
            return Ok(());
        }
        Ok(CliAction::ShowHelp) => {
            println!("{}", help_text());
            return Ok(());
        }
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
        Ok(CliAction::RunServer) => {}
    }

    // Logs go to stderr so they don't corrupt the stdio MCP protocol on stdout.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let service = TuiServer::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_combo_plain_key() {
        let (key, mods) = parse_combo("enter");
        assert_eq!(key, "enter");
        assert!(!mods.ctrl && !mods.alt && !mods.shift);
    }

    #[test]
    fn parse_combo_with_modifiers() {
        let (key, mods) = parse_combo("ctrl+shift+a");
        assert_eq!(key, "a");
        assert!(mods.ctrl && mods.shift && !mods.alt);
    }

    #[test]
    fn parse_combo_single_plus_is_literal() {
        // A lone "+" has no modifiers and is treated as the key itself.
        let (key, _mods) = parse_combo("+");
        assert_eq!(key, "+");
    }

    #[test]
    fn screen_format_selects_ansi() {
        assert!(matches!(screen_format(Some("ansi")), ScreenFormat::Ansi));
        assert!(matches!(screen_format(Some("text")), ScreenFormat::Text));
        assert!(matches!(screen_format(None), ScreenFormat::Text));
    }

    // --- CLI argument classification (pure `parse_cli_args`) ---

    #[test]
    fn no_args_runs_server() {
        let empty: [&str; 0] = [];
        assert_eq!(parse_cli_args(empty), Ok(CliAction::RunServer));
    }

    #[test]
    fn version_flag_is_classified() {
        assert_eq!(parse_cli_args(["--version"]), Ok(CliAction::ShowVersion));
        assert_eq!(parse_cli_args(["-V"]), Ok(CliAction::ShowVersion));
    }

    #[test]
    fn help_flag_is_classified() {
        assert_eq!(parse_cli_args(["--help"]), Ok(CliAction::ShowHelp));
        assert_eq!(parse_cli_args(["-h"]), Ok(CliAction::ShowHelp));
    }

    #[test]
    fn unknown_arg_is_error() {
        let err = parse_cli_args(["--nope"]).unwrap_err();
        assert!(err.contains("unknown argument"));
        assert!(err.contains("--nope"));
    }

    // --- CLI output text (built directly, independent of classification) ---

    #[test]
    fn version_line_has_name_and_version() {
        let v = version_line();
        assert!(v.contains(env!("CARGO_PKG_NAME")));
        assert!(v.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn help_text_has_usage_and_flags() {
        let h = help_text();
        assert!(h.contains("USAGE"));
        assert!(h.contains("--help"));
        assert!(h.contains("--version"));
    }
}
