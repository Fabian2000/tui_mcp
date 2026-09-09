//! Development-only tools, compiled in by the `dev-tools` feature.
//!
//! The whole module is gated at its `mod` declaration because `#[tool_router]`
//! emits a registration for every `#[tool]` method it can see and does *not*
//! evaluate `#[cfg]` on them: gating a single method inside the main impl block
//! leaves the router calling a function that no longer exists. Gating an entire
//! block -- here, an entire file -- works, because then the macro never runs.
//!
//! The tools live in their own `dev_tool_router`, which `TuiServer::router()`
//! merges into the main one.

use rmcp::model::CallToolResult;
use rmcp::{ErrorData as McpError, tool, tool_router};

use crate::{TuiServer, reply, version_line};

/// Cargo features compiled into this binary, in manifest order.
///
/// Rust cannot enumerate enabled features at runtime, and reading the manifest
/// would describe the source tree rather than the running image -- the two
/// disagree exactly when this tool matters. So the table is written by hand and
/// each entry resolves through `cfg!`; the `features_match_manifest` test fails
/// the build if it ever falls behind `[features]` in Cargo.toml.
const FEATURES: &[(&str, bool)] = &[("dev-tools", cfg!(feature = "dev-tools"))];

// `vis` must be a string literal: the attribute is parsed by darling, which
// reads a visibility out of a literal and rejects bare tokens.
#[tool_router(router = dev_tool_router, vis = "pub(crate)")]
impl TuiServer {
    /// Report the identity of the running image, so a caller can tell which
    /// build is answering.
    ///
    /// A server over stdio never hot-reloads: a rebuild replaces the file on
    /// disk while the client keeps talking to the process it already spawned,
    /// so the server answering can be older than the source with nothing in the
    /// source to show it. The crate version does not settle it either, since
    /// that only moves at release time. The running image does: its path, and
    /// the mtime of the file at that path.
    ///
    /// That mtime describes the file at the path, not the loaded image, so the
    /// two part company as soon as a rebuild replaces or removes that file. On
    /// Linux it always does: cargo unlinks the binary rather than overwriting
    /// it, leaving the path reported as `... (deleted)` -- the kernel's
    /// annotation for an unlinked `/proc/pid/exe` -- and the mtime unreadable.
    /// That pair names a stale process more plainly than any timestamp could,
    /// so reaching for a platform-specific handle to the loaded image would
    /// replace a clear answer with a subtler one.
    #[tool(
        description = "Report which binary is serving this session: executable path and \
        mtime, server version and process uptime. Use it after reloading the server to \
        confirm the running process picked up your rebuild."
    )]
    async fn dev_info(&self) -> Result<CallToolResult, McpError> {
        let now = std::time::SystemTime::now();
        let stamp =
            |t: std::time::SystemTime| format!("{} ({} ago)", format_utc(t), age_since(t, now));

        let exe = std::env::current_exe();
        let path = match &exe {
            Ok(p) => p.display().to_string(),
            Err(e) => format!("<unavailable: {e}>"),
        };
        let mtime = exe
            .ok()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
            .map_or_else(|| "<unavailable>".to_string(), stamp);

        let features: Vec<&str> = FEATURES
            .iter()
            .filter(|(_, on)| *on)
            .map(|(name, _)| *name)
            .collect();
        let features = if features.is_empty() {
            "<none>".to_string()
        } else {
            features.join(", ")
        };

        Ok(reply(format!(
            "{version}\n  \
             executable:       {path}\n  \
             binary mtime:     {mtime}\n  \
             started:          {started}\n  \
             features:         {features}\n  \
             target:           {arch} {os} ({env})\n  \
             family:           {family}\n  \
             debug_assertions: {debug}\n  \
             protocol:         MCP {proto}",
            version = version_line(),
            started = stamp(self.started_at),
            arch = std::env::consts::ARCH,
            os = std::env::consts::OS,
            env = if cfg!(target_env = "msvc") {
                "msvc"
            } else if cfg!(target_env = "gnu") {
                "gnu"
            } else if cfg!(target_env = "musl") {
                "musl"
            } else {
                // `target_env` is empty on targets with no distinct C environment.
                "no env"
            },
            // Reported on its own rather than folded into the line above, where
            // it would repeat `os` verbatim on Windows. Worth the line because
            // it still classifies an `os` value the reader does not recognize --
            // which is the case when the server runs somewhere the client does
            // not, in a VM or a container.
            family = std::env::consts::FAMILY,
            debug = if cfg!(debug_assertions) { "on" } else { "off" },
            proto = rmcp::model::ProtocolVersion::LATEST,
        )))
    }
}

/// `2026-09-05 15:04:12 UTC`.
///
/// UTC rather than local time on purpose: this timestamp gets compared against
/// file listings, logs and other machines, and one that silently means
/// something different depending on where it is read is worse than one that
/// always means the same instant.
fn format_utc(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t)
        .format("%Y-%m-%d %H:%M:%S UTC")
        .to_string()
}

/// How long ago `then` was, relative to `now`, as `1h02m03s` / `4m12s` / `7s`.
///
/// Shown next to the absolute timestamp because it answers a different
/// question: whether this image predates the last build, without the reader
/// doing arithmetic. A `then` in the future (clock skew, or a file whose
/// preserved timestamp came from a machine running fast) reads as `0s` rather
/// than failing -- being off by seconds does not change that answer.
fn age_since(then: std::time::SystemTime, now: std::time::SystemTime) -> String {
    let secs = now
        .duration_since(then)
        .unwrap_or(std::time::Duration::ZERO)
        .as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins `FEATURES` to the manifest. Hand-parsing `[features]` is crude, but
    /// the crate has no TOML dependency and a key list needs no more.
    #[test]
    fn features_match_manifest() {
        let manifest = include_str!("../Cargo.toml");
        let declared: Vec<&str> = manifest
            .lines()
            .skip_while(|l| l.trim() != "[features]")
            .skip(1)
            .take_while(|l| !l.trim_start().starts_with('['))
            .filter_map(|l| l.split_once('='))
            .map(|(k, _)| k.trim())
            .filter(|k| !k.is_empty() && !k.starts_with('#'))
            .collect();

        assert!(
            !declared.is_empty(),
            "parsed no features -- has the [features] table moved or been renamed?"
        );
        for name in declared {
            assert!(
                FEATURES.iter().any(|(n, _)| *n == name),
                "feature `{name}` is in Cargo.toml but missing from FEATURES, \
                 so dev_info would not report it"
            );
        }
    }

    /// chrono owns the calendar arithmetic; what is ours is the layout, so this
    /// pins the field order, zero padding and suffix rather than re-testing that
    /// leap years work.
    #[test]
    fn format_utc_lays_out_the_timestamp() {
        use std::time::{Duration, UNIX_EPOCH};
        let at = |s: u64| format_utc(UNIX_EPOCH + Duration::from_secs(s));

        assert_eq!(at(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(at(1_000_000_000), "2001-09-09 01:46:40 UTC");
    }

    #[test]
    fn age_since_formats_by_magnitude_and_clamps_the_future() {
        use std::time::{Duration, SystemTime};
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let age = |secs: u64| age_since(t0, t0 + Duration::from_secs(secs));

        assert_eq!(age(7), "7s");
        assert_eq!(age(252), "4m12s");
        assert_eq!(age(3723), "1h02m03s");
        // Seconds and minutes stay zero-padded so widths line up between calls.
        assert_eq!(age(3600), "1h00m00s");
        // A timestamp ahead of `now` must not panic on the subtraction.
        assert_eq!(age_since(t0 + Duration::from_secs(60), t0), "0s");
    }
}
