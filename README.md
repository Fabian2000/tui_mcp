# tui_mcp

MCP server for driving and watching terminal programs.

You start a program under a real PTY. The server feeds that program's output into an embedded `vt100` emulator and keeps the screen grid it builds. Tools send keyboard and mouse input, then read the grid back as text. Your own terminal stays untouched. Raw mode, the alternate screen, and a program that exits and starts another child are byte streams the emulator already understands.

The server speaks MCP over stdio and opens no network ports.

## Install and register

```bash
cargo build --release
```

Point the client at the binary you just built, using its absolute path. On Windows that file is `target\release\tui_mcp.exe`. Elsewhere it is `target/release/tui_mcp`.

JSON config, used by Claude Code, Cursor, VS Code, Windsurf, and others:

```json
{
  "mcpServers": {
    "tui": { "command": "/path/to/tui_mcp/target/release/tui_mcp" }
  }
}
```

TOML config. For the Codex CLI, add this to `~/.codex/config.toml`:

```toml
[mcp_servers.tui]
command = "/path/to/tui_mcp/target/release/tui_mcp"
```

Some clients can write the entry for you:

```bash
# Claude Code
claude mcp add tui -- /path/to/tui_mcp/target/release/tui_mcp

# Codex CLI
codex mcp add tui -- /path/to/tui_mcp/target/release/tui_mcp
```

Check the client's docs for the exact file and format.

## A typical session

1. `session_start` with a name and a command.
2. `wait_for_text` or `wait_for_stable` until the UI is ready.
3. `send_key`, `send_keys`, `send_text`, `paste`, or `send_mouse`.
4. `read_screen` to see what happened.
5. `session_stop` when you are done. `session_purge` removes every session whose program has already exited.

## Tools

| Tool | Purpose |
|------|---------|
| `session_start` | Spawn a program. `mode: "pty"` (default, real TUI) or `"piped"` (separate stdout/stderr, no TTY). |
| `session_list` | List sessions with kind, command, cwd, and exit status. |
| `session_stop` | Kill and remove a session. |
| `session_purge` | Remove all sessions whose program has already exited. |
| `signal` | Send an OS signal (`INT`, `TERM`, `HUP`, `TSTP`, `CONT`, `WINCH`, `USR1`, ...) to a session's process. Unix only. |
| `send_key` | Named key plus ctrl/alt/shift modifiers, with an optional repeat `count`. |
| `send_keys` | A sequence of key presses in one call, such as `["ctrl+c", "enter"]`. Optional `delay_ms`. |
| `send_text` | Type literal text. Optional `delay_ms` paces the characters. |
| `paste` | Paste multi-line text. Wraps it in bracketed-paste markers when the program has enabled that mode. |
| `send_bytes` | Raw bytes, with `\n` `\r` `\t` `\e` `\xHH` escapes, for input the key and mouse tools do not cover. |
| `send_mouse` | Click, scroll, drag, or hover at 1-based `(x, y)`, using SGR mouse reporting. `count` repeats the action (`count: 2` is a double-click). `delay_ms` spaces the repeats. |
| `read_screen` | Dump the PTY screen as text or ANSI, with size and cursor. This is the one to use by default. |
| `screenshot` | PNG of the PTY screen, for color and layout checks. Costs far more tokens than `read_screen`. |
| `screenshot_to_file` | Write that PNG to a file and return a short confirmation. For a person or for docs, since it spends no image tokens. Never overwrites: on a name collision it writes an indexed name such as `shot(1).png`. |
| `read_scrollback` | Paged scrollback (the visible screen plus lines that scrolled off), with line numbers. |
| `search_scrollback` | Regex search over scrollback. Returns matching lines, with optional context. |
| `read_output` | Read buffered stdout or stderr of a piped session. `clear` drains the buffer. |
| `close_stdin` | Send EOF to a piped program. |
| `resize` | Resize a PTY (`cols` by `rows`). |
| `wait_for_text` | Block until text appears. `regex` matches a pattern. `absent` waits until it disappears. `format` chooses the returned screen: `none`, `text`, or `ansi`. |
| `wait_for_stable` | Block until the screen stops changing, or until timeout. `format` is `none`, `text`, or `ansi`. |
| `wait_for_change` | Block until the screen differs from its current contents. `format` is `none`, `text`, or `ansi`. If it never changes, the result is the status line alone. |
| `wait_for_exit` | Block until the program exits. Returns the exit status for a PTY or a piped session. |
| `wait_for_output` | Block until a piped session's stdout or stderr contains text or a regex match. |
| `sleep` | Fixed wait. Capped by `max_ms`, and never longer than 60 seconds. |

## Sessions

`session_start` runs the command in one of two modes.

| Mode | When to use it |
|------|----------------|
| `pty` (default) | Real terminal, for TUIs. Default size is 80 columns by 24 rows. |
| `piped` | Separate stdout and stderr, no TTY. For ordinary commands. |

`signal` can deliver `INT`, `TERM`, `HUP`, `TSTP`, `CONT`, `WINCH`, `USR1`, and other OS signals. Unix only.

## Keyboard, mouse, and paste

### Key names

A key is a single character, or one of `enter`, `tab`, `esc`, `backspace`, `delete`, `insert`, `home`, `end`, `pageup`, `pagedown`, `up`, `down`, `left`, `right`, `f1` through `f12`, `space`.

Modifiers are `ctrl`, `alt`, and `shift`. For example, `key="c"` with `modifiers=["ctrl"]`.

`send_keys` takes the same names with modifiers joined by `+`, such as `["ctrl+c", "enter", "alt+shift+f"]`. `delay_ms` pauses between presses. On `send_text`, `delay_ms` pauses between characters, which helps programs that drop a fast burst.

### Kitty keyboard protocol

If a program enables the [Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/), the server sees the flags query on the program's output and answers it, so the program actually turns the protocol on. `send_key` then encodes modified keys in Kitty form. `Ctrl+C` goes out as `CSI 99;5u`, which keeps that chord distinct from a plain `C`. The tool result includes `(kitty)` when this encoding was used.

Arrows, F-keys, Home, and End keep their usual CSI sequences. A program that never enables the protocol still receives legacy xterm encoding.

### Multi-line paste

Typing a multi-line block with `send_text` is a series of keystrokes. Editors auto-indent each line, and a leading character can run a command.

`paste` sends the block as one paste. When the program has enabled bracketed paste (`CSI ? 2004 h`), the server wraps the text in `ESC[200~` ... `ESC[201~`, and editors such as vim and nano insert it verbatim. If that mode is off, `paste` sends the text as plain input and says so in the result.

### Mouse

`send_mouse` clicks, scrolls, drags, or hovers at a 1-based `(x, y)` with SGR mouse reporting. `count: 2` is a double-click. `delay_ms` spaces repeated actions. `resize` sets the PTY to `cols` by `rows`.

## Reading output

### Text and screenshots

`read_screen` returns plain text, plus the size and cursor. Use it unless you specifically need color. `format: "ansi"` adds ANSI color and attribute escapes.

`screenshot` renders the same grid to a PNG with the real foreground and background colors. That image costs many more tokens, so keep it for a color or layout question that the text cannot answer.

`screenshot_to_file` writes the same PNG and returns a short confirmation, which spends no image tokens. Use it when the picture is for a person or for documentation. If the path already exists, the file is saved under an indexed name (`shot(1).png`, `shot(2).png`, ...) and the result names the path that was written.

### Hyperlinks

When a program emits OSC 8 hyperlinks, `format: "ansi"` adds a `Hyperlinks:` footnote. The footnote lists URIs whose display text is currently on screen. Links are matched by that display text, because the emulator does not track the cell they occupy. A link drops out of the footnote as soon as a later read no longer finds its text. The same words showing up on another line later do not bring it back.

### Scrollback

The visible screen is only `rows` by `cols`. Lines that scroll off the top stay in a 5,000-line buffer.

`read_scrollback` returns that history one page at a time. The default page is 100 lines, most recent first, with line numbers, so a long history does not flood the context.

`search_scrollback` runs a regex over the whole buffer and returns the matching lines, with optional surrounding context.

Full-screen programs such as vim and htop usually leave this buffer empty. Read those with `read_screen`.

### Piped stdout and stderr

Each piped session keeps stdout and stderr in a ring buffer, 2 MiB per stream by default. `buffer_bytes` on `session_start` changes the cap. Once a stream grows past the cap, the oldest bytes are dropped. The next `read_output` starts with a notice like this, so a short buffer is obvious:

```
[tui_mcp: N earlier byte(s) dropped ...]
```

For a long-running program that prints often, call `read_output` frequently and pass `clear: true` to drain what you just read. `close_stdin` sends EOF. `wait_for_output` blocks until stdout or stderr contains a string or a regex match.

## Waiting

`wait_for_text`, `wait_for_stable`, and `wait_for_change` take a `format` that chooses the payload once the wait ends.

| `format` | Result |
|----------|--------|
| `text` | Plain-text screen. This is the default, and omitting `format` does the same thing. |
| `ansi` | The screen with ANSI color and attribute escapes, same as `read_screen` with `format: "ansi"`. |
| `none` | The outcome line only. Examples: `matched in ~40ms`, `gone in ~40ms`, `changed in ~40ms`, `stable in ~350ms`, or a `TIMEOUT ...` line. Use this when you only need the wait to finish. |

Matching always uses the plain-text screen, so `format` changes the payload and leaves the match condition alone. `text` and `ansi` are captured from the frame that satisfied the wait. A UI that redraws immediately cannot substitute a later frame.

A successful result includes how long the wait took (`in ~{N}ms`). The clock starts when the tool begins polling, and the number is rounded to the poll interval. Treat it as a rough latency from the call.

`wait_for_text` accepts `regex` for a pattern, and `absent` to wait until the text is gone.

On `wait_for_change`, a timeout means the screen still matches what you already had. The result is the outcome line alone, for every `format`. The unchanged screen is left out.

`wait_for_exit` returns the exit status for either session kind. `sleep` waits a fixed time, limited by `max_ms`, with a hard cap of 60 seconds.

## Development builds

`dev-tools` exists for work on `tui_mcp` itself. Release builds leave it off, so a published binary does not include it.

```bash
cargo build --features dev-tools
```

The feature adds `dev_info`, which reports the running executable's path and mtime, plus version, enabled features, target, and uptime.

A server on stdio does not pick up a rebuild by itself. Cargo replaces the file on disk, and the client keeps talking to the process it already spawned. A launcher that starts a snapshot copy makes this easier to miss, because the live process is a different file from the one you just built. `dev_info` asks that process to name itself.

Sample output:

```
tui_mcp 1.3.0
  executable:       /home/dev/tui_mcp/target/debug/tui_mcp
  binary mtime:     2026-09-05 17:21:18 UTC (2m26s ago)
  started:          2026-09-05 17:23:45 UTC (0s ago)
  features:         dev-tools
  target:           x86_64 linux (gnu)
  family:           unix
  debug_assertions: on
  protocol:         MCP 2025-03-26
```

`binary mtime` is the modification time of the file at the executable's path. Compare it with the time of your last build. Timestamps are UTC, so they line up with file listings and logs from any machine.

The timestamp follows that path. It matches the loaded image only while the file there is still the one this process started from. A rebuild that replaces or removes the file splits the two apart.

On Linux the split is easy to see. Cargo unlinks the old binary and links a new one, so a process still running the old build reports:

```
  executable:       /home/dev/tui_mcp/target/debug/tui_mcp (deleted)
  binary mtime:     <unavailable>
  started:          2026-09-05 17:26:15 UTC (1m58s ago)
```

`(deleted)` is the kernel's note on `/proc/<pid>/exe` once that path has been unlinked. The process is serving a binary that is no longer a file on disk, which is why `binary mtime` is unavailable.

A snapshot launcher, or any platform that leaves the mapped file in place, keeps the path as it was. Nothing in the output then marks the process as stale. The mtime is the clue, and it will be the build from before last.

The readings above were checked on Linux and Windows. macOS resolves the executable path by a different mechanism, so which of the two readings a stale process gets there is still unknown.

## Security

The server runs whatever command the connected client asks for, as the user account that started it, with that process's environment and working directory. That is what it is for. Connect it only to clients you trust, and run it with no more privilege than the task needs. It opens no network ports of its own.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

The bundled font `assets/DejaVuSansMono.ttf` is distributed under the Bitstream Vera license. See [`assets/DejaVuSansMono.LICENSE.txt`](assets/DejaVuSansMono.LICENSE.txt).
