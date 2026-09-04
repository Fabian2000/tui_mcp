//! Stamps a Windows version resource (`VERSIONINFO`) into the executable.
//!
//! Everything is derived from `Cargo.toml`; this is best effort, so a missing
//! resource compiler downgrades to a warning rather than failing the build.
//!
//! Cargo always runs a build script — there is no way to declare one per
//! platform — so on a non-Windows host this compiles down to an empty `main`.

#[cfg(not(windows))]
fn main() {}

// Item position: include! cannot expand items inside a function body.
#[cfg(windows)]
include!("src/display_name.rs");

/// Publisher shown by Explorer, Task Manager and UAC elevation prompts.
///
/// The Win32 spec marks CompanyName as required, but there is no company or
/// publisher behind this project to claim, and an invented one would be worse
/// than none. Maintainer: set this to `Some("...")` -- a name or handle -- and
/// it is stamped into every Windows build; leave it `None` to omit the field.
#[cfg(windows)]
const COMPANY_NAME: Option<&str> = None;

#[cfg(windows)]
fn main() {
    // Without these, Cargo falls back to scanning the whole package, and with
    // only build.rs listed a version bump in Cargo.toml would leave a stale
    // resource behind.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src/display_name.rs");

    // A Windows host can still be building for something else.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    use std::env;
    use winresource::{VersionInfo, WindowsResource};

    // `new()` already fills ProductName from the package name and both version
    // strings from the full package version.
    let mut res = WindowsResource::new();

    // FileDescription is a short label shown to users -- Task Manager treats it
    // as the app name -- so it takes the display title, matching the `title`
    // that get_info() reports over MCP. The prose belongs in Comments, which is
    // specified as "additional information ... for diagnostic purposes".
    res.set("FileDescription", DISPLAY_NAME);
    if let Some(company) = COMPANY_NAME {
        res.set("CompanyName", company);
    }
    if let Ok(description) = env::var("CARGO_PKG_DESCRIPTION")
        && !description.is_empty()
    {
        res.set("Comments", &description);
    }
    if let Ok(license) = env::var("CARGO_PKG_LICENSE")
        && !license.is_empty()
    {
        res.set("LegalCopyright", &license);
    }

    // The numeric FILEVERSION is four 16-bit words, so it carries only
    // MAJOR.MINOR.PATCH; the string field keeps the full version verbatim.
    // Anything trailing the numbers means this is not an upstream release:
    // rc/beta are pre-releases, anything else is a private build.
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let numeric = format!(
        "{}.{}.{}",
        env::var("CARGO_PKG_VERSION_MAJOR").unwrap_or_default(),
        env::var("CARGO_PKG_VERSION_MINOR").unwrap_or_default(),
        env::var("CARGO_PKG_VERSION_PATCH").unwrap_or_default(),
    );
    let suffix = version.strip_prefix(&numeric).unwrap_or_default();
    if !suffix.is_empty() {
        let suffix = suffix.to_ascii_lowercase();
        if suffix.contains("rc") || suffix.contains("beta") {
            res.set_version_info(VersionInfo::FILEFLAGS, VersionInfo::VS_FF_PRERELEASE);
        } else {
            // VS_FF_PRIVATEBUILD requires the PrivateBuild string to be set.
            res.set("PrivateBuild", &version);
            res.set_version_info(VersionInfo::FILEFLAGS, VersionInfo::VS_FF_PRIVATEBUILD);
        }
    }

    if let Err(e) = res.compile() {
        println!("cargo:warning=skipping Windows version resource: {e}");
    }
}
