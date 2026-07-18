# tui_mcp

An MCP server for remote-controlling and observing TUI programs. It runs a
program under a real PTY with an embedded terminal emulator (`vt100`). You drive
the program with simulated keyboard and mouse input, and read the rendered
screen back as text. This works even when the program toggles raw mode, switches
to the alternate screen, or restarts a child at runtime.

## Why a PTY plus emulator

You never touch the controlling terminal. The program writes bytes to a pseudo
terminal, and those bytes are fed to a `vt100` parser that keeps a virtual
screen grid. Reading the screen just dumps that grid. That sidesteps the usual
raw-mode testing pain. Raw and cooked toggles, alternate-screen switches, and
programs that come and go are all just byte streams the emulator already
understands.

## Build

```bash
cargo build --release
```

## Register with an MCP client

`tui_mcp` speaks MCP over stdio, so any MCP-capable client can run it. Point the
client at the binary you built, using the absolute path to
`target/release/tui_mcp`. Configure it the way your client expects. Two common
formats are shown below.

Many clients (Claude Code, Cursor, VS Code, Windsurf, and others) use a JSON
config that lists MCP servers:

```json
{
  "mcpServers": {
    "tui": { "command": "/path/to/tui_mcp/target/release/tui_mcp" }
  }
}
```

Others use a TOML config. For the Codex CLI, add this to `~/.codex/config.toml`:

```toml
[mcp_servers.tui]
command = "/path/to/tui_mcp/target/release/tui_mcp"
```

Some clients also provide a command-line helper that writes the config for you:

```bash
# Claude Code
claude mcp add tui -- /path/to/tui_mcp/target/release/tui_mcp

# Codex CLI
codex mcp add tui -- /path/to/tui_mcp/target/release/tui_mcp
```

Check your client's own documentation for the exact location and format.

## Tools

| Tool | Purpose |
|------|---------|
| `session_start` | Spawn a program. `mode: "pty"` (default, real TUI) or `"piped"` (separate stdout/stderr, no TTY). |
| `session_list` | List sessions with kind, command, cwd and exit status. |
| `session_stop` | Kill and remove a session. |
| `session_purge` | Remove all sessions whose program has already exited. |
| `signal` | Send an OS signal (INT, TERM, HUP, TSTP, CONT, WINCH, USR1, ...) to a session's process (Unix). |
| `send_key` | Named key plus ctrl/alt/shift modifiers, optional repeat `count`. |
| `send_keys` | A sequence of key presses in one call, e.g. `["ctrl+c", "enter"]` (optional `delay_ms`). |
| `send_text` | Type literal text (optional `delay_ms` to pace characters). |
| `paste` | Paste multi-line text. Wraps in bracketed-paste markers when the program enabled the mode. |
| `send_bytes` | Raw bytes with `\n \r \t \e \xHH` escapes, for anything the key and mouse tools don't cover. |
| `send_mouse` | Click, scroll, drag, or hover at 1-based `(x, y)` (SGR mouse reporting). `count` repeats the action (`count: 2` = double-click); `delay_ms` spaces the repeats. |
| `read_screen` | Dump the pty screen as text or ANSI, with size and cursor. Preferred and cheapest. |
| `screenshot` | PNG of the pty screen for color and layout checks. Costlier, so use it only when colors matter. |
| `read_scrollback` | Paged scrollback history (visible plus scrolled-off lines) with line numbers. |
| `search_scrollback` | Regex search over scrollback. Returns matching lines with optional context. |
| `read_output` | Read buffered stdout/stderr of a piped session (`clear` to drain). |
| `close_stdin` | Send EOF to a piped program. |
| `resize` | Resize a pty (cols by rows). |
| `wait_for_text` | Block until text appears on screen. `regex` matches a pattern, `absent` waits for it to disappear. |
| `wait_for_stable` | Block until the screen stops changing (with timeout). |
| `wait_for_change` | Block until the screen changes from its current contents. |
| `wait_for_exit` | Block until the program exits. Returns the exit status (pty or piped). |
| `wait_for_output` | Block until a piped session's stdout/stderr contains text or a regex match. |
| `sleep` | Fixed wait, capped by `max_ms` (hard cap 60 s). |

## Kitty keyboard protocol

If a program enables the [kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
(as some modern TUIs do), the server notices it on the program's output stream
and answers the flags query so the program actually turns it on. `send_key` then
encodes keys in the kitty form, so `Ctrl+C` becomes `CSI 99;5u`. That cleanly
disambiguates modified keys. Programs that do not use it are unaffected and get
the legacy xterm encoding. Functional keys such as arrows, F-keys, and Home/End
keep their standard CSI forms. `send_key` adds `(kitty)` to its result when it
used the kitty encoding.

## Pasting multi-line text

Typing a multi-line block with `send_text` makes editors treat each line as
keystrokes. Auto-indent then shifts your code, and leading characters can
trigger commands. The `paste` tool avoids that. The server notices when a
program has enabled bracketed paste mode (`CSI ? 2004 h` on its output) and wraps
the text in `ESC[200~ ... ESC[201~`, so editors like vim and nano insert it
verbatim. If the program has not enabled the mode, `paste` sends the text as-is
and says so in its result.

## Key names

Single characters, or one of: `enter`, `tab`, `esc`, `backspace`, `delete`,
`insert`, `home`, `end`, `pageup`, `pagedown`, `up`, `down`, `left`, `right`,
`f1` through `f12`, `space`. Modifiers combine, for example
`key="c", modifiers=["ctrl"]`.

## Reading the screen: text vs screenshot

`read_screen` returns plain text and is the cheapest way to observe a TUI, so use
it by default. `screenshot` renders the screen to a PNG with the real foreground
and background colors, which costs far more tokens. Reach for it only when a
color or layout question cannot be answered from text alone.

## Scrollback

The visible screen is only `rows` by `cols`. Lines that scroll off the top are
kept in an internal scrollback buffer of 5000 lines. `read_scrollback` returns
that history one page at a time (100 lines per page by default, most recent
first) so it does not flood context. `search_scrollback` runs a regex over the
whole history and returns just the matching lines. Full-screen TUIs like vim and
htop usually have no scrollback, so use `read_screen` there.

## Output buffering (piped mode)

Each piped session keeps its stdout and stderr in a bounded ring buffer, 2 MiB
per stream by default and configurable with `buffer_bytes` on `session_start`.
If a program produces more than that between reads, the oldest bytes are dropped.
`read_output` then prepends a notice like
`[tui_mcp: N earlier byte(s) dropped ...]` so the truncated output is never
mistaken for the program's complete output. For chatty long-running programs,
read often and pass `clear: true`.

## Security model

This server runs whatever commands the connected client asks it to, in the
server's own user account, environment, and working directory. That is the whole
point of it. Only connect it to clients you trust, and run it with no more
privilege than the tasks need. It opens no network ports of its own.

## Typical flow

1. `session_start { name, command }`
2. `wait_for_text` or `wait_for_stable` to sync
3. `send_key`, `send_keys`, `send_text`, or `send_mouse`
4. `read_screen` to observe
5. `session_stop` (or `session_purge`) when done

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. The bundled font `assets/DejaVuSansMono.ttf` is distributed
under the Bitstream Vera license. See
[`assets/DejaVuSansMono.LICENSE.txt`](assets/DejaVuSansMono.LICENSE.txt).
