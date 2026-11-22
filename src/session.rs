//! Session management: spawn a child program either under a real PTY (so it
//! behaves as if attached to a terminal, full TUI support) or with plain
//! pipes (separate stdout/stderr, no TTY).
//!
//! For PTY sessions the child's output is continuously parsed by a `vt100`
//! terminal emulator, so the on-screen grid can be queried as text at any time.
//! Whether the child toggles raw mode, switches to the alternate screen, exits
//! and restarts a child. It does not matter, we only feed bytes to the
//! emulator and read the resulting screen.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

/// Options shared by both spawn modes.
pub struct SpawnOpts {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
    pub cols: u16,
    pub rows: u16,
    /// Per-stream output cap for piped sessions, in bytes (ring buffer).
    pub buffer_bytes: usize,
}

/// Number of scrollback lines retained by the terminal emulator (pty mode).
pub const PTY_SCROLLBACK_LINES: usize = 5000;

pub enum Session {
    Pty(PtySession),
    Piped(PipedSession),
}

pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    child: Box<dyn Child + Send + Sync>,
    _reader: JoinHandle<()>,
    cols: u16,
    rows: u16,
    cmdline: String,
    cwd: String,
    kitty: crate::kitty::KittyFlags,
    paste: crate::kitty::PasteFlag,
    hyperlinks: crate::osc8::HyperlinkLog,
}

pub struct PipedSession {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Arc<Mutex<StreamBuf>>,
    stderr: Arc<Mutex<StreamBuf>>,
    _out_reader: JoinHandle<()>,
    _err_reader: JoinHandle<()>,
    cmdline: String,
    cwd: String,
}

/// A bounded output buffer. When the accumulated data exceeds `cap`, the oldest
/// bytes are dropped and counted, so readers can be told data was lost rather
/// than silently receiving a truncated stream.
pub struct StreamBuf {
    data: Vec<u8>,
    dropped: u64,
    cap: usize,
}

impl StreamBuf {
    fn new(cap: usize) -> Self {
        Self {
            data: Vec::new(),
            dropped: 0,
            cap: cap.max(4096),
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        if self.data.len() > self.cap {
            let overflow = self.data.len() - self.cap;
            self.data.drain(..overflow);
            self.dropped += overflow as u64;
        }
    }
}

/// A snapshot of a PTY screen.
pub struct ScreenDump {
    pub text: String,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_hidden: bool,
    pub rows: u16,
    pub cols: u16,
}

impl ScreenDump {
    /// Format the dump as a human-readable block with a leading status line.
    /// Size is width x height. Cursor is x (column) and y (row), both 0-based.
    pub fn render(&self) -> String {
        format!(
            "[size {}W x {}H] cursor=(x{}, y{}){}\n{}",
            self.cols,
            self.rows,
            self.cursor_col,
            self.cursor_row,
            if self.cursor_hidden { " hidden" } else { "" },
            self.text
        )
    }
}

/// Human-readable command line for display, e.g. `vim -u NONE "my file.txt"`.
fn format_cmdline(opts: &SpawnOpts) -> String {
    let mut parts = vec![opts.command.clone()];
    for arg in &opts.args {
        if arg.is_empty() || arg.chars().any(char::is_whitespace) {
            parts.push(format!("\"{arg}\""));
        } else {
            parts.push(arg.clone());
        }
    }
    parts.join(" ")
}

/// The working directory the child will actually run in: the explicit `cwd` if
/// given, otherwise the server's current directory (so it can be reproduced).
/// It must be an existing folder, or the spawn is refused.
///
/// Both spawn paths pass the result on explicitly, never leaving it unset:
/// portable-pty (0.9.0, `CommandBuilder`) starts a child with no `cwd`, or
/// one that fails `is_dir()`, in the home folder (`USERPROFILE` on Windows,
/// `HOME` or the passwd entry on Unix), not in the server's folder. Without
/// the check a missing folder would silently become the home folder in pty
/// mode, while a piped spawn fails with a vaguer OS error.
///
/// Kept as a path, not a string: `display()` replaces bytes that aren't valid
/// UTF-8, so a server folder with such a name would not be found again.
fn resolve_cwd(opts: &SpawnOpts) -> Result<PathBuf> {
    let dir = match &opts.cwd {
        Some(cwd) => PathBuf::from(cwd),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    if !dir.is_dir() {
        bail!("cwd is not an existing folder: {}", dir.display());
    }
    Ok(dir)
}

fn build_command(opts: &SpawnOpts, cwd: &Path) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(&opts.command);
    cmd.args(&opts.args);
    cmd.cwd(cwd);
    for (k, v) in &opts.env {
        cmd.env(k, v);
    }
    cmd
}

/// How a program that exited on its own is reported, the same for pty and
/// piped sessions: `exit code: 3`. A code with the high bit set is a Windows
/// NTSTATUS (0xC000013A after Ctrl+C, 0xC0000005 after a crash) and is shown
/// in hex, as std does on Windows, since it is looked up that way.
fn exit_code_text(code: u32) -> String {
    if code & 0x8000_0000 == 0 {
        format!("exit code: {code}")
    } else {
        format!("exit code: {code:#x}")
    }
}

/// The reply to a cursor-position report request (DSR `CSI 6 n`): the terminal
/// answers `CSI row;col R`, 1-based. `row`/`col` are the emulator's 0-based
/// cursor position.
fn cursor_position_report(row: u16, col: u16) -> String {
    format!("\x1b[{};{}R", row + 1, col + 1)
}

impl PtySession {
    pub fn spawn(opts: &SpawnOpts) -> Result<Self> {
        let cwd = resolve_cwd(opts)?;
        let pty_system = portable_pty::native_pty_system();
        let size = PtySize {
            rows: opts.rows,
            cols: opts.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system.openpty(size).context("failed to open pty")?;

        let cmd = build_command(opts, &cwd);
        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn command under pty")?;
        // Drop the slave so the master sees EOF when the child exits.
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            opts.rows,
            opts.cols,
            PTY_SCROLLBACK_LINES,
        )));
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone pty reader")?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .context("failed to take pty writer")?,
        ));

        let kitty: crate::kitty::KittyFlags = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let paste: crate::kitty::PasteFlag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hyperlinks: crate::osc8::HyperlinkLog = Arc::new(Mutex::new(Vec::new()));
        let parser_for_thread = Arc::clone(&parser);
        let writer_for_thread = Arc::clone(&writer);
        let mut detector = crate::kitty::KittyDetector::new(Arc::clone(&kitty), Arc::clone(&paste));
        let mut osc8 = crate::osc8::Osc8Tracker::new(Arc::clone(&hyperlinks));
        let reader_handle = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // Watch for kitty keyboard-protocol negotiation and answer
                        // flag queries, then hand the bytes to the emulator.
                        let reply = detector.feed(&buf[..n]);
                        if !reply.is_empty() {
                            let mut w = writer_for_thread.lock();
                            let _ = w.write_all(&reply);
                            let _ = w.flush();
                        }
                        // Answer a cursor-position report request (DSR `CSI 6 n`)
                        // with the emulator's current cursor. Without this, ConPTY
                        // on Windows (created with PSEUDOCONSOLE_INHERIT_CURSOR)
                        // blocks at startup and no interactive program ever renders.
                        //
                        // The position is read before this chunk is processed, so a
                        // DSR arriving mid-chunk (after cursor movement in the same
                        // read) is answered with the pre-chunk position. That is exact
                        // for the ConPTY startup handshake, where the DSR is the first
                        // output, and only imprecise for a program that moves the
                        // cursor and queries within a single write burst.
                        if detector.take_cursor_report_request() {
                            let (row, col) = parser_for_thread.lock().screen().cursor_position();
                            let resp = cursor_position_report(row, col);
                            let mut w = writer_for_thread.lock();
                            let _ = w.write_all(resp.as_bytes());
                            let _ = w.flush();
                        }
                        // Update the screen and log the chunk's hyperlinks under one
                        // lock, emulator first. A read that landed between the two
                        // would otherwise see a link logged but not yet drawn, and
                        // `visible_footnote` would retire it for good.
                        {
                            let mut parser = parser_for_thread.lock();
                            parser.process(&buf[..n]);
                            osc8.feed(&buf[..n]);
                        }
                    }
                }
            }
        });

        Ok(PtySession {
            master: pair.master,
            writer,
            parser,
            child,
            _reader: reader_handle,
            cols: opts.cols,
            rows: opts.rows,
            cmdline: format_cmdline(opts),
            cwd: cwd.display().to_string(),
            kitty,
            paste,
            hyperlinks,
        })
    }

    /// Current kitty keyboard-protocol flags (0 = legacy encoding).
    pub fn kitty_flags(&self) -> u8 {
        self.kitty.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the program has enabled bracketed paste mode.
    pub fn paste_enabled(&self) -> bool {
        self.paste.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// OS process id of the child, if still known.
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    pub fn cmdline(&self) -> &str {
        &self.cmdline
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let mut w = self.writer.lock();
        w.write_all(bytes).context("pty write failed")?;
        w.flush().ok();
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize failed")?;
        self.parser.lock().screen_mut().set_size(rows, cols);
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    pub fn dump(&self, format: ScreenFormat) -> ScreenDump {
        let parser = self.parser.lock();
        let screen = parser.screen();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let (rows, cols) = screen.size();
        let text = match format {
            ScreenFormat::Text => screen.contents(),
            ScreenFormat::Ansi => {
                let mut ansi = String::from_utf8_lossy(&screen.contents_formatted()).into_owned();
                let mut links = self.hyperlinks.lock();
                if let Some(footnote) =
                    crate::osc8::visible_footnote(&screen.contents(), &mut links)
                {
                    ansi.push_str("\n\n");
                    ansi.push_str(&footnote);
                }
                ansi
            }
        };
        ScreenDump {
            text,
            cursor_row,
            cursor_col,
            cursor_hidden: screen.hide_cursor(),
            rows,
            cols,
        }
    }

    /// Render the visible screen to a PNG (for color / layout inspection).
    pub fn screenshot(&self) -> Result<Vec<u8>> {
        let parser = self.parser.lock();
        crate::render::screen_to_png(parser.screen())
    }

    /// Full logical history (scrollback + visible), one entry per line, with
    /// trailing blank lines trimmed. The emulator keeps this internally. We
    /// reconstruct it by walking the scrollback offset.
    pub fn scrollback_lines(&self) -> Vec<String> {
        let mut parser = self.parser.lock();
        let (rows, cols) = parser.screen().size();
        let rows = rows as usize;

        parser.screen_mut().set_scrollback(usize::MAX);
        let above = parser.screen().scrollback();
        let total = above + rows;
        let mut lines = vec![String::new(); total];

        let mut k = above;
        loop {
            parser.screen_mut().set_scrollback(k);
            for (i, text) in parser.screen().rows(0, cols).enumerate() {
                let abs = above - k + i;
                if abs < lines.len() {
                    lines[abs] = text;
                }
            }
            if k == 0 {
                break;
            }
            k = k.saturating_sub(rows);
        }
        parser.screen_mut().set_scrollback(0);

        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        lines
    }

    /// Exit status if the child has terminated, `None` if still running.
    /// Worded like the piped one: see `exit_code_text`.
    pub fn exit_status(&mut self) -> Option<String> {
        match self.child.try_wait() {
            // portable-pty keeps only `strsignal()`'s description of a signal
            // ("Killed", "Interrupt"), not its number, and sets the code to a
            // made-up 1 then, so the code is left out.
            Ok(Some(status)) => Some(match status.signal() {
                Some(sig) => format!("signal: {sig}"),
                None => exit_code_text(status.exit_code()),
            }),
            _ => None,
        }
    }

    pub fn kill(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

#[derive(Clone, Copy)]
pub enum ScreenFormat {
    Text,
    Ansi,
}

impl PipedSession {
    pub fn spawn(opts: &SpawnOpts) -> Result<Self> {
        use std::process::{Command, Stdio};
        let cwd = resolve_cwd(opts)?;
        let mut cmd = Command::new(&opts.command);
        cmd.args(&opts.args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().context("failed to spawn piped command")?;

        let stdin = child.stdin.take();
        let stdout = Arc::new(Mutex::new(StreamBuf::new(opts.buffer_bytes)));
        let stderr = Arc::new(Mutex::new(StreamBuf::new(opts.buffer_bytes)));

        let mut out = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let mut err = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

        let out_buf = Arc::clone(&stdout);
        let out_reader = std::thread::spawn(move || pump(&mut out, &out_buf));
        let err_buf = Arc::clone(&stderr);
        let err_reader = std::thread::spawn(move || pump(&mut err, &err_buf));

        Ok(PipedSession {
            child,
            stdin,
            stdout,
            stderr,
            _out_reader: out_reader,
            _err_reader: err_reader,
            cmdline: format_cmdline(opts),
            cwd: cwd.display().to_string(),
        })
    }

    pub fn cmdline(&self) -> &str {
        &self.cmdline
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("stdin is closed"))?;
        stdin.write_all(bytes).context("stdin write failed")?;
        stdin.flush().ok();
        Ok(())
    }

    /// Close the child's stdin (signals EOF to the program).
    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    /// OS process id of the child.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn read_stream(&self, which: Stream, clear: bool) -> String {
        let buf = match which {
            Stream::Stdout => &self.stdout,
            Stream::Stderr => &self.stderr,
        };
        let mut guard = buf.lock();
        let mut s = String::from_utf8_lossy(&guard.data).into_owned();
        if guard.dropped > 0 {
            // Tell the caller the stream overflowed so it isn't mistaken for the
            // program's real, complete output.
            s = format!(
                "[tui_mcp: {} earlier byte(s) dropped. Output buffer capped at {} bytes, \
                 read more often with clear=true to avoid loss]\n{}",
                guard.dropped, guard.cap, s
            );
        }
        if clear {
            guard.data.clear();
        }
        s
    }

    /// Exit status if the child has terminated, `None` if still running.
    /// Worded like the pty one: see `exit_code_text`.
    pub fn exit_status(&mut self) -> Option<String> {
        match self.child.try_wait() {
            // std words an exit code per platform ("exit code: 3" on Windows,
            // "exit status: 3" on Unix), so only its signal text is kept:
            // "signal: 9 (SIGKILL)".
            Ok(Some(status)) => Some(match status.code() {
                Some(code) => exit_code_text(code.cast_unsigned()),
                None => status.to_string(),
            }),
            _ => None,
        }
    }

    pub fn kill(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

#[derive(Clone, Copy)]
pub enum Stream {
    Stdout,
    Stderr,
}

fn pump<R: Read>(reader: &mut R, buf: &Arc<Mutex<StreamBuf>>) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.lock().push(&chunk[..n]),
        }
    }
}

/// Thread-safe registry of named sessions.
#[derive(Clone, Default)]
pub struct SessionManager {
    inner: Arc<Mutex<HashMap<String, Session>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a session, returning `(command line, working directory)` so the
    /// caller can report exactly what was started.
    pub fn create(
        &self,
        name: String,
        mode_pty: bool,
        opts: &SpawnOpts,
    ) -> Result<(String, String)> {
        let mut map = self.inner.lock();
        if map.contains_key(&name) {
            bail!("a session named '{name}' already exists");
        }
        let (session, cmdline, cwd) = if mode_pty {
            let s = PtySession::spawn(opts)?;
            let (c, w) = (s.cmdline().to_string(), s.cwd().to_string());
            (Session::Pty(s), c, w)
        } else {
            let s = PipedSession::spawn(opts)?;
            let (c, w) = (s.cmdline().to_string(), s.cwd().to_string());
            (Session::Piped(s), c, w)
        };
        map.insert(name, session);
        Ok((cmdline, cwd))
    }

    /// Run `f` against a named session, or error if it doesn't exist.
    pub fn with<T>(&self, name: &str, f: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
        let mut map = self.inner.lock();
        let session = map
            .get_mut(name)
            .ok_or_else(|| anyhow!("no session named '{name}'"))?;
        f(session)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let mut map = self.inner.lock();
        let mut session = map
            .remove(name)
            .ok_or_else(|| anyhow!("no session named '{name}'"))?;
        match &mut session {
            Session::Pty(s) => s.kill(),
            Session::Piped(s) => s.kill(),
        }
        Ok(())
    }

    /// Remove and kill every session whose program has already exited.
    /// Returns the names that were purged.
    pub fn purge_exited(&self) -> Vec<String> {
        let mut map = self.inner.lock();
        let exited: Vec<String> = map
            .iter_mut()
            .filter_map(|(name, session)| {
                let done = match session {
                    Session::Pty(s) => s.exit_status().is_some(),
                    Session::Piped(s) => s.exit_status().is_some(),
                };
                done.then(|| name.clone())
            })
            .collect();
        for name in &exited {
            if let Some(mut s) = map.remove(name) {
                match &mut s {
                    Session::Pty(p) => p.kill(),
                    Session::Piped(p) => p.kill(),
                }
            }
        }
        let mut sorted = exited;
        sorted.sort();
        sorted
    }

    /// One line item per session for `session_list`.
    pub fn list(&self) -> Vec<SessionInfo> {
        let mut map = self.inner.lock();
        let mut out: Vec<SessionInfo> = map
            .iter_mut()
            .map(|(name, session)| {
                let (kind, pid, cmdline, cwd, status) = match session {
                    Session::Pty(s) => (
                        "pty",
                        s.pid(),
                        s.cmdline().to_string(),
                        s.cwd().to_string(),
                        s.exit_status(),
                    ),
                    Session::Piped(s) => (
                        "piped",
                        Some(s.pid()),
                        s.cmdline().to_string(),
                        s.cwd().to_string(),
                        s.exit_status(),
                    ),
                };
                SessionInfo {
                    name: name.clone(),
                    kind,
                    pid,
                    cmdline,
                    cwd,
                    status,
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Send an OS signal to a session's process (Unix). Returns the pid signalled.
    #[cfg(unix)]
    pub fn signal(&self, name: &str, sig: nix::sys::signal::Signal) -> Result<u32> {
        let pid = self.with(name, |s| {
            Ok(match s {
                Session::Pty(p) => p.pid(),
                Session::Piped(p) => Some(p.pid()),
            })
        })?;
        let pid = pid.ok_or_else(|| anyhow!("session '{name}' has no live process"))?;
        let raw = i32::try_from(pid).map_err(|_| anyhow!("pid {pid} out of range"))?;
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), sig)
            .with_context(|| format!("failed to send signal to pid {pid}"))?;
        Ok(pid)
    }

    /// Type Ctrl+C into a session's program (Windows, which has no signals).
    ///
    /// In a pty, Ctrl+C is the byte 0x03, which ConPTY turns into a
    /// CTRL_C_EVENT for the console's programs. It is written raw, not through
    /// the key encoding: once a program enables the kitty keyboard protocol,
    /// ctrl+c is encoded as `ESC[99;5u`, which interrupted nothing in tests
    /// (2026-11-22), while a raw 0x03 interrupted console programs and Claude
    /// Code alike. A pipe has no console to turn the byte into an event, so a
    /// piped session is refused.
    #[cfg(not(unix))]
    pub fn interrupt(&self, name: &str) -> Result<()> {
        self.with(name, |s| match s {
            Session::Pty(p) => {
                // A write to a program that has exited succeeds silently.
                if p.exit_status().is_some() {
                    bail!("the program in session '{name}' has already exited");
                }
                p.write(&[0x03])
            }
            Session::Piped(_) => bail!(
                "a piped session cannot be interrupted on Windows: start the program \
                 in pty mode, or use session_stop"
            ),
        })
    }
}

/// A single session's metadata, as reported by [`SessionManager::list`].
pub struct SessionInfo {
    pub name: String,
    pub kind: &'static str,
    pub pid: Option<u32>,
    pub cmdline: String,
    pub cwd: String,
    pub status: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_report_is_one_based_row_then_col() {
        // DSR reply is `CSI row;col R`, 1-based, row before col.
        assert_eq!(cursor_position_report(0, 0), "\x1b[1;1R");
        assert_eq!(cursor_position_report(24, 79), "\x1b[25;80R");
    }

    #[test]
    fn exit_code_text_is_decimal_unless_the_high_bit_is_set() {
        assert_eq!(exit_code_text(0), "exit code: 0");
        assert_eq!(exit_code_text(3), "exit code: 3");
        assert_eq!(exit_code_text(0xC000_013A), "exit code: 0xc000013a");
    }

    /// Options for a program that exits with code 3 at once.
    fn exit_3_opts() -> SpawnOpts {
        #[cfg(windows)]
        let (command, args) = ("cmd.exe", ["/c", "exit 3"]);
        #[cfg(unix)]
        let (command, args) = ("sh", ["-c", "exit 3"]);
        SpawnOpts {
            command: command.to_string(),
            args: args.map(str::to_string).to_vec(),
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
            buffer_bytes: 1024,
        }
    }

    /// Poll `status` for up to ~5 s until the program has exited.
    pub(super) fn wait_exited(mut status: impl FnMut() -> Option<String>) -> String {
        for _ in 0..100 {
            if let Some(st) = status() {
                return st;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("program did not exit within 5 s");
    }

    /// Options for a program that prints its working folder, in `cwd`.
    fn print_cwd_opts(cwd: Option<String>) -> SpawnOpts {
        #[cfg(windows)]
        let (command, args) = ("cmd.exe", ["/c", "cd"]);
        #[cfg(unix)]
        let (command, args) = ("sh", ["-c", "pwd -P"]);
        SpawnOpts {
            command: command.to_string(),
            args: args.map(str::to_string).to_vec(),
            cwd,
            env: HashMap::new(),
            cols: 200,
            rows: 24,
            buffer_bytes: 4096,
        }
    }

    /// Poll `text` for up to ~5 s until it contains `want`; returns the last text.
    fn wait_contains(want: &str, mut text: impl FnMut() -> String) -> String {
        let mut last = String::new();
        for _ in 0..100 {
            last = text();
            if last.contains(want) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        last
    }

    /// Regression test: with no `cwd`, portable-pty started a pty child in the
    /// home folder while `session_start` reported the server's folder.
    #[test]
    fn a_child_without_cwd_starts_in_the_servers_folder() {
        let here = std::env::current_dir()
            .expect("current dir")
            .display()
            .to_string();

        let mut pty = PtySession::spawn(&print_cwd_opts(None)).expect("spawn under pty");
        let screen = wait_contains(&here, || pty.dump(ScreenFormat::Text).text);
        pty.kill();
        assert!(
            screen.contains(&here),
            "pty child printed {screen:?}, expected {here:?}"
        );
        assert_eq!(pty.cwd(), here);

        let piped = PipedSession::spawn(&print_cwd_opts(None)).expect("spawn piped");
        let out = wait_contains(&here, || piped.read_stream(Stream::Stdout, false));
        assert!(
            out.contains(&here),
            "piped child printed {out:?}, expected {here:?}"
        );
        assert_eq!(piped.cwd(), here);
    }

    /// A `cwd` that does not exist is refused in both modes, instead of the pty
    /// child silently starting in the home folder.
    #[test]
    fn a_missing_cwd_is_refused() {
        let missing = std::env::temp_dir()
            .join(format!("tui-mcp-no-such-folder-{}", std::process::id()))
            .display()
            .to_string();
        let opts = print_cwd_opts(Some(missing.clone()));

        let Err(pty) = PtySession::spawn(&opts) else {
            panic!("pty spawn in {missing} should fail");
        };
        let Err(piped) = PipedSession::spawn(&opts) else {
            panic!("piped spawn in {missing} should fail");
        };
        for e in [pty, piped] {
            assert_eq!(
                e.to_string(),
                format!("cwd is not an existing folder: {missing}")
            );
        }
    }

    /// A piped program waiting for a line gets it from the Enter key: `\n`,
    /// not the `\r` a terminal sends (which no line reader ends a line on).
    #[test]
    fn a_piped_program_reads_a_line_ended_by_enter() {
        #[cfg(windows)]
        let (command, args) = ("cmd.exe", vec!["/v:on", "/c", "set /p L=& echo got:!L!."]);
        #[cfg(unix)]
        let (command, args) = ("sh", vec!["-c", "read L; echo got:$L."]);
        let opts = SpawnOpts {
            command: command.to_string(),
            args: args.into_iter().map(str::to_string).collect(),
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
            buffer_bytes: 4096,
        };
        let mut piped = PipedSession::spawn(&opts).expect("spawn piped");
        let enter = crate::keys::piped_key_to_bytes("enter", crate::keys::Mods::default())
            .expect("enter is a key");
        piped.write(b"abc").expect("write text");
        piped.write(&enter).expect("write enter");

        let out = wait_contains("got:abc.", || piped.read_stream(Stream::Stdout, false));
        piped.kill();
        assert!(out.contains("got:abc."), "piped program printed {out:?}");
    }

    #[test]
    fn pty_and_piped_word_an_exit_code_the_same() {
        let mut pty = PtySession::spawn(&exit_3_opts()).expect("spawn under pty");
        let pty_status = wait_exited(|| pty.exit_status());
        let mut piped = PipedSession::spawn(&exit_3_opts()).expect("spawn piped");
        let piped_status = wait_exited(|| piped.exit_status());

        assert_eq!(pty_status, "exit code: 3");
        assert_eq!(piped_status, "exit code: 3");
    }
}

/// Windows counterpart to `unix_pty_tests`: the same pty behavior, verified
/// through a native Windows harness (ConPTY + cmd.exe) instead of bash.
#[cfg(all(test, windows))]
mod windows_pty_tests {
    use super::*;

    /// Regression test for issue #2: interactive pty mode was broken on native
    /// Windows because ConPTY (created with `PSEUDOCONSOLE_INHERIT_CURSOR`)
    /// blocks until its startup cursor-position DSR (`CSI 6 n`) is answered.
    /// `cmd.exe` must render its prompt once the reader answers that DSR.
    #[test]
    fn interactive_cmd_renders_under_pty() {
        let opts = SpawnOpts {
            command: "cmd.exe".to_string(),
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 25,
            buffer_bytes: 0,
        };
        let mut session = PtySession::spawn(&opts).expect("spawn cmd.exe under pty");

        // Poll for up to ~5s; in practice the prompt appears within ~200ms.
        let mut rendered = String::new();
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let dump = session.dump(ScreenFormat::Text);
            if !dump.text.trim().is_empty() {
                rendered = dump.text;
                break;
            }
        }
        session.kill();

        assert!(
            !rendered.trim().is_empty(),
            "cmd.exe never rendered under pty — ConPTY cursor DSR likely unanswered (issue #2)"
        );
    }
}

#[cfg(all(test, unix))]
mod unix_pty_tests {
    use super::*;

    /// A program killed by a signal reads `signal: ...` in both modes, with no
    /// exit code beside it (portable-pty's code is a made-up 1 then).
    #[test]
    fn a_signal_death_is_worded_as_a_signal() {
        let opts = || SpawnOpts {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "kill -9 $$".to_string()],
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
            buffer_bytes: 1024,
        };
        let mut pty = PtySession::spawn(&opts()).expect("spawn under pty");
        let pty_status = super::tests::wait_exited(|| pty.exit_status());
        let mut piped = PipedSession::spawn(&opts()).expect("spawn piped");
        let piped_status = super::tests::wait_exited(|| piped.exit_status());

        assert!(
            pty_status.starts_with("signal: ") && !pty_status.contains("code"),
            "pty: {pty_status:?}"
        );
        assert_eq!(piped_status, "signal: 9 (SIGKILL)");
    }

    /// Unix counterpart to `interactive_cmd_renders_under_pty`: a program that
    /// emits a cursor-position request (DSR `CSI 6 n`) on its output must get
    /// `CSI row;col R` back on its input. Answering it is what stops
    /// prompt_toolkit REPLs (ptpython/IPython) from warning "terminal doesn't
    /// support CPR" and laying out wrong — and it needs no such program to test.
    ///
    /// A tiny `bash` child switches its tty to raw mode (so its `read` returns
    /// on the reply's `R` rather than waiting for a newline, and the reply
    /// isn't echoed back), emits the DSR, reads the terminal's answer, and
    /// prints it as printable text `CPR[row;col]`. `${rep#*[}` drops the
    /// leading `ESC[`. If the reader thread never answers, `read` blocks
    /// forever and the marker never appears (the pre-fix behavior).
    #[test]
    fn cpr_request_is_answered_over_pty() {
        let script = r#"stty raw -echo; printf '\033[6n'; IFS= read -r -s -d R rep; printf 'CPR[%s]\n' "${rep#*[}""#;
        let opts = SpawnOpts {
            command: "bash".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
            buffer_bytes: 0,
        };
        let mut session = PtySession::spawn(&opts).expect("spawn bash under pty");

        // Poll for up to ~3s; the round trip completes within a few ms.
        let mut marker = String::new();
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let text = session.dump(ScreenFormat::Text).text;
            if let Some(inside) = text
                .split_once("CPR[")
                .and_then(|(_, rest)| rest.split_once(']'))
                .map(|(inside, _)| inside.to_string())
            {
                marker = inside;
                break;
            }
        }
        session.kill();

        // The child only prints after its `read` returns, so getting a
        // well-formed `row;col` at all proves the DSR was answered. It never
        // moved the cursor, so the emulator reports home (1;1, 1-based).
        let (row, col) = marker.split_once(';').unwrap_or(("", ""));
        assert!(
            !row.is_empty()
                && row.bytes().all(|b| b.is_ascii_digit())
                && !col.is_empty()
                && col.bytes().all(|b| b.is_ascii_digit()),
            "expected the terminal to answer the cursor-position request with row;col, \
             got marker {marker:?} — DSR likely unanswered"
        );
        assert_eq!(marker, "1;1", "cursor should report home before any output");
    }

    /// Stronger than the home-position test: it proves the reply carries the
    /// *actual* cursor, not a hardcoded value. The child moves the cursor to a
    /// known spot (`CSI 5;10 H`) and only then queries, so the answer must be
    /// `5;10`; a constant `1;1` reply (ignoring the real cursor) would fail.
    ///
    /// The move and the query are sent as two writes with a gap between them.
    /// The reader thread reads the cursor position *before* applying the
    /// current chunk (see `PtySession::spawn`), so the move must land in an
    /// earlier read than the query — otherwise the answer would reflect the
    /// pre-move (home) position. The `sleep` forces that separation.
    #[test]
    fn cpr_reports_the_actual_cursor_position() {
        let script = r#"stty raw -echo; printf '\033[5;10H'; sleep 0.1; printf '\033[6n'; IFS= read -r -s -d R rep; printf 'CPR[%s]\n' "${rep#*[}""#;
        let opts = SpawnOpts {
            command: "bash".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
            buffer_bytes: 0,
        };
        let mut session = PtySession::spawn(&opts).expect("spawn bash under pty");

        let mut marker = String::new();
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let text = session.dump(ScreenFormat::Text).text;
            if let Some(inside) = text
                .split_once("CPR[")
                .and_then(|(_, rest)| rest.split_once(']'))
                .map(|(inside, _)| inside.to_string())
            {
                marker = inside;
                break;
            }
        }
        session.kill();

        // CSI 5;10 H put the cursor at row 5, col 10 (1-based), so the DSR
        // answer must report exactly that.
        assert_eq!(
            marker, "5;10",
            "cursor-position report did not reflect the real cursor (CSI 5;10 H)"
        );
    }
}
