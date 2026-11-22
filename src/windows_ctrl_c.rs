//! Windows: let Ctrl+C reach the programs the server starts.
//!
//! A Windows process can carry an "ignore Ctrl+C" flag, and every process it
//! starts inherits it. Git Bash (MSYS2) sets it for the native programs it
//! runs, so a server started from there (an MCP client in a Git Bash
//! terminal, or `cargo test` in one) would pass it on to every session's
//! program: a Ctrl+C typed into a pty (`send_bytes "\x03"`, `send_key
//! ctrl+c`) becomes a CTRL_C_EVENT that the program ignores.
//! Measured 2026-11-22: `ping -n 30` under a pty was not interrupted when
//! the server inherited the flag, and was when it didn't.
//!
//! `SetConsoleCtrlHandler(NULL, FALSE)` clears the flag for this process,
//! so children don't inherit it. That alone would also let a Ctrl+C in
//! a console the server shares with its client kill the server, so a handler
//! is registered that makes the server itself ignore Ctrl+C and Ctrl+Break,
//! whether or not it inherited the flag. A server run by hand in a console
//! therefore does not stop on Ctrl+C; close its stdin or kill it instead.
//! Handlers are not inherited, so children keep the default.
//!
//! The handler handles only Ctrl+C and Ctrl+Break, so the client still
//! controls the server's lifetime. A stdio MCP client stops a server by
//! closing its stdin (rmcp's `waiting()` returns and `main` ends) or by
//! killing it, which on Windows is `TerminateProcess` (also what Node's
//! `child.kill()` does, whatever signal is named) and can't be intercepted.
//! Console close, logoff and shutdown events are left to the default
//! handling.
//!
//! Ignoring Ctrl+C matters when the server shares a console with its
//! client: a Ctrl+C pressed there (in `claude` CLI to interrupt a turn, say)
//! is delivered to every process in the console, and would end a server
//! that did not inherit the flag (one started from PowerShell, say).
//! Whether a client and the server ever share a console is not verified.

unsafe extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

const CTRL_C_EVENT: u32 = 0;
const CTRL_BREAK_EVENT: u32 = 1;

/// Handles Ctrl+C and Ctrl+Break for the server itself by ignoring them;
/// other events (close, logoff, shutdown) go to the next handler.
unsafe extern "system" fn ignore_interrupts(ctrl_type: u32) -> i32 {
    i32::from(ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT)
}

/// Clear the inherited "ignore Ctrl+C" flag, so the programs started from
/// now on get Ctrl+C, while the server itself ignores it. Returns
/// `false` if either call failed.
pub fn pass_to_children() -> bool {
    // SAFETY: plain Win32 calls with no pointers; the handler is a
    // `'static` function with the signature `PHANDLER_ROUTINE` expects.
    unsafe {
        let cleared = SetConsoleCtrlHandler(None, 0) != 0;
        let handled = SetConsoleCtrlHandler(Some(ignore_interrupts), 1) != 0;
        cleared && handled
    }
}
