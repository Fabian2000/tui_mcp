//! The environment a started program gets: the server's own, minus the
//! variables that describe the MCP client's session, plus the caller's `env`.
//! Shared by both spawn modes (pty and piped).

use std::collections::HashMap;

use portable_pty::CommandBuilder;

/// Variables that the MCP client sets about its own session or account, which a
/// started program should not inherit: they describe the agent's session, not
/// the program's, and some are private (a messaging token, the user's email).
/// A nested `claude` started with them would also act as part of the host
/// session (its id, its messaging socket, its entrypoint).
///
/// This is a fixed list, not a name prefix, so variables the user set (even with
/// the same prefix) still reach the program. `CLAUDECODE=1` is deliberately not
/// here: it only says the program runs under Claude Code, which stays true.
/// Variables from other MCP clients can be added to this list.
///
/// Names are matched case-insensitively on Windows: both `env_remove`
/// implementations below fold case there.
const STRIPPED_CLIENT_ENV: &[&str] = &[
    // When this list changes (another client added, names added or removed),
    // check the `env` description of `session_start` (`StartArgs::env` in
    // main.rs): it tells agents which client's variables are not inherited.

    // Claude Code: the host session's identity and messaging.
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_LAUNCHED_BY_SESSION_IDS",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_CHILD_NOTES_DIR",
    "CLAUDE_CODE_ENTRYPOINT",
    // Claude Code (set by the desktop app): the signed-in account.
    "CLAUDE_CODE_USER_EMAIL",
    "CLAUDE_CODE_ACCOUNT_UUID",
    "CLAUDE_CODE_ORGANIZATION_UUID",
    // Claude Code (set by the desktop app): its host refreshes the session's
    // auth tokens, which no host does for a started program.
    "CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH",
];

/// Lets one `apply_env` work for both spawn modes. portable-pty's
/// `CommandBuilder` (pty) and `std::process::Command` (piped) both have
/// `env` and `env_remove` with the same meaning, but as separate inherent
/// methods with no shared trait, so generic code can't call them directly.
pub trait ChildEnv {
    fn set_var(&mut self, key: &str, value: &str);
    fn remove_var(&mut self, key: &str);
}

impl ChildEnv for CommandBuilder {
    fn set_var(&mut self, key: &str, value: &str) {
        self.env(key, value);
    }
    fn remove_var(&mut self, key: &str) {
        // Also covers variables portable-pty adds from the Windows registry.
        self.env_remove(key);
    }
}

impl ChildEnv for std::process::Command {
    fn set_var(&mut self, key: &str, value: &str) {
        self.env(key, value);
    }
    fn remove_var(&mut self, key: &str) {
        self.env_remove(key);
    }
}

/// Build the child's environment: the inherited one minus
/// [`STRIPPED_CLIENT_ENV`], then the caller's `env`, where `Some` sets a
/// variable and `None` removes it. The caller's entries are applied last, so
/// they win, including for a stripped name.
pub fn apply_env(cmd: &mut impl ChildEnv, env: &HashMap<String, Option<String>>) {
    for name in STRIPPED_CLIENT_ENV {
        cmd.remove_var(name);
    }
    for (k, v) in env {
        match v {
            Some(v) => cmd.set_var(k, v),
            None => cmd.remove_var(k),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, Option<&str>)]) -> HashMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.map(str::to_string)))
            .collect()
    }

    /// `std::process::Command` records removals as `(key, None)`. Names match
    /// the way the platform compares them: ignoring case only on Windows.
    fn std_env_value(cmd: &std::process::Command, key: &str) -> Option<Option<String>> {
        cmd.get_envs()
            .find(|(k, _)| {
                if cfg!(windows) {
                    k.eq_ignore_ascii_case(key)
                } else {
                    **k == *key
                }
            })
            .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    }

    #[test]
    fn pty_env_strips_client_session_vars() {
        let mut cmd = CommandBuilder::new("prog");
        // Stand-ins for inherited values: `apply_env` removes a name wherever
        // it came from.
        cmd.env("CLAUDE_CODE_SESSION_ID", "host-session");
        cmd.env("CLAUDE_CODE_USER_EMAIL", "someone@example.com");
        cmd.env("CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH", "1");
        cmd.env("CLAUDECODE", "1");
        cmd.env("CLAUDE_CODE_SOMETHING_USER_SET", "kept");
        apply_env(&mut cmd, &HashMap::new());
        assert_eq!(cmd.get_env("CLAUDE_CODE_SESSION_ID"), None);
        assert_eq!(cmd.get_env("CLAUDE_CODE_USER_EMAIL"), None);
        assert_eq!(cmd.get_env("CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH"), None);
        assert_eq!(cmd.get_env("CLAUDECODE"), Some("1".as_ref()));
        assert_eq!(
            cmd.get_env("CLAUDE_CODE_SOMETHING_USER_SET"),
            Some("kept".as_ref())
        );
    }

    #[test]
    fn pty_env_explicit_value_wins_over_stripping() {
        let mut cmd = CommandBuilder::new("prog");
        cmd.env("CLAUDE_CODE_ENTRYPOINT", "claude-desktop");
        apply_env(
            &mut cmd,
            &env_of(&[
                ("CLAUDE_CODE_ENTRYPOINT", Some("cli")),
                ("FOO", Some("bar")),
            ]),
        );
        assert_eq!(cmd.get_env("CLAUDE_CODE_ENTRYPOINT"), Some("cli".as_ref()));
        assert_eq!(cmd.get_env("FOO"), Some("bar".as_ref()));
    }

    #[test]
    fn piped_env_strips_client_session_vars() {
        let mut cmd = std::process::Command::new("prog");
        cmd.env("CLAUDE_CODE_MESSAGING_TOKEN", "secret");
        apply_env(&mut cmd, &HashMap::new());
        assert_eq!(
            std_env_value(&cmd, "CLAUDE_CODE_MESSAGING_TOKEN"),
            Some(None)
        );
        assert_eq!(std_env_value(&cmd, "CLAUDECODE"), None);
    }

    #[test]
    fn piped_env_explicit_value_wins_over_stripping() {
        let mut cmd = std::process::Command::new("prog");
        apply_env(
            &mut cmd,
            &env_of(&[("CLAUDE_CODE_SESSION_ID", Some("chosen"))]),
        );
        assert_eq!(
            std_env_value(&cmd, "CLAUDE_CODE_SESSION_ID"),
            Some(Some("chosen".to_string()))
        );
    }

    #[test]
    fn pty_env_null_removes_a_variable() {
        let mut cmd = CommandBuilder::new("prog");
        // Stand-in for an inherited variable the caller wants gone.
        cmd.env("CLAUDECODE", "1");
        cmd.env("KEEP_ME", "yes");
        apply_env(
            &mut cmd,
            &env_of(&[("CLAUDECODE", None), ("EMPTY", Some(""))]),
        );
        assert_eq!(cmd.get_env("CLAUDECODE"), None);
        assert_eq!(cmd.get_env("KEEP_ME"), Some("yes".as_ref()));
        // An empty string still sets the variable, to empty.
        assert_eq!(cmd.get_env("EMPTY"), Some("".as_ref()));
    }

    #[test]
    fn piped_env_null_removes_a_variable() {
        let mut cmd = std::process::Command::new("prog");
        cmd.env("CLAUDECODE", "1");
        apply_env(
            &mut cmd,
            &env_of(&[("CLAUDECODE", None), ("EMPTY", Some(""))]),
        );
        assert_eq!(std_env_value(&cmd, "CLAUDECODE"), Some(None));
        assert_eq!(std_env_value(&cmd, "EMPTY"), Some(Some(String::new())));
    }

    /// Windows environment names are case-insensitive, so a differently cased
    /// copy of a stripped name must not slip through.
    #[cfg(windows)]
    #[test]
    fn env_stripping_ignores_case_on_windows() {
        let mut pty = CommandBuilder::new("prog");
        pty.env("claude_code_session_id", "host-session");
        apply_env(&mut pty, &HashMap::new());
        assert_eq!(pty.get_env("CLAUDE_CODE_SESSION_ID"), None);

        let mut piped = std::process::Command::new("prog");
        piped.env("claude_code_session_id", "host-session");
        apply_env(&mut piped, &HashMap::new());
        assert_eq!(std_env_value(&piped, "CLAUDE_CODE_SESSION_ID"), Some(None));
    }
}
