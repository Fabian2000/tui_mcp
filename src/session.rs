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
fn resolve_cwd(opts: &SpawnOpts) -> String {
    opts.cwd.clone().unwrap_or_else(|| {
        std::env::current_dir().map_or_else(|_| ".".to_string(), |p| p.display().to_string())
    })
}

fn build_command(opts: &SpawnOpts) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(&opts.command);
    cmd.args(&opts.args);
    if let Some(cwd) = &opts.cwd {
        cmd.cwd(cwd);
    }
    for (k, v) in &opts.env {
        cmd.env(k, v);
    }
    cmd
}

/// The reply to a cursor-position report request (DSR `CSI 6 n`): the terminal
/// answers `CSI row;col R`, 1-based. `row`/`col` are the emulator's 0-based
/// cursor position.
fn cursor_position_report(row: u16, col: u16) -> String {
    format!("\x1b[{};{}R", row + 1, col + 1)
}

impl PtySession {
    pub fn spawn(opts: &SpawnOpts) -> Result<Self> {
        let pty_system = portable_pty::native_pty_system();
        let size = PtySize {
            rows: opts.rows,
            cols: opts.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system.openpty(size).context("failed to open pty")?;

        let cmd = build_command(opts);
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
        let parser_for_thread = Arc::clone(&parser);
        let writer_for_thread = Arc::clone(&writer);
        let mut detector = crate::kitty::KittyDetector::new(Arc::clone(&kitty), Arc::clone(&paste));
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
                        parser_for_thread.lock().process(&buf[..n]);
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
            cwd: resolve_cwd(opts),
            kitty,
            paste,
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
                String::from_utf8_lossy(&screen.contents_formatted()).into_owned()
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
    pub fn exit_status(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(format!("{status:?}")),
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
        let mut cmd = Command::new(&opts.command);
        cmd.args(&opts.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &opts.cwd {
            cmd.current_dir(cwd);
        }
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
            cwd: resolve_cwd(opts),
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

    pub fn exit_status(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
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
}

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
