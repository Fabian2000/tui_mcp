# Changelog

<!--
New contributions should add a changelog entry to the `[Unreleased]` section at the top of this file.
If there is no `[Unreleased]` section, create a new one.
Don't add PR numbers; the GitHub release notes link every PR.
The repo maintainer cuts releases and bumps the version themself.
-->

Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- `send_text`: `submit` presses Enter after the text
- `signal INT` on Windows types Ctrl+C in terminal mode; other signals are refused with an explanation

### Changed
- `wait_for_text` and `wait_for_output`: `^` and `$` match per line, `\A` and `\z` the whole text
- Exit status is worded the same in terminal and pipes mode
- The schema publishes the real parameter defaults instead of `null`
- Tool descriptions cover the common traps (submitting Enter, per-OS `signal` and `resize`, regex anchors)

### Fixed
- A terminal-mode program starts in the reported folder; a `cwd` that does not exist is refused
- `send_key enter` sends `\n` in pipes mode
- Windows: Ctrl+C reaches programs when the server is started from Git Bash

## 1.3.0 - 2026-10-12

### Added
- `dev_info` reports which binary is serving
- Windows version resource stamped into the executable
- Linux arm64 build and test in CI

### Changed
- Linux binaries built on Ubuntu 22.04 (glibc 2.35)

## 1.2.1 - 2026-09-01

### Added
- `screenshot_to_file` indexes the file name on collision

### Fixed
- `read_screen` drops hyperlinks once they leave the screen

## 1.2.0 - 2026-08-14

### Added
- `screenshot_to_file` tool
- `read_screen` shows OSC 8 hyperlinks in `ansi` output

## 1.1.0 - 2026-07-26

### Added
- `send_mouse` accepts `count` and `delay_ms` for double-clicks
- `format` and elapsed time on the wait tools
- `--version` and `--help` answer instead of blocking on stdio

### Fixed
- Interactive mode on Windows: ConPTY cursor position request is answered
- `serverInfo` reports the real name, version and repository URL

## 1.0.0 - 2026-07-07

- Initial release.
