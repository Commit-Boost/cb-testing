//! Root-cause extraction from a crashed service's logs (Task 2, the heart).
//!
//! `extract_root_cause` is PURE: log text in, an optional structured cause out.
//! It is pattern-based, not string-match-based — it captures the varying field
//! name / message so held-out crashes (a field it has never seen) are diagnosed
//! the same as the ones we captured today. The `triage` entry point owns all the
//! process I/O (kurtosis / docker); this module never touches the outside world.
//!
//! Why this exists: `kurtosis service logs` routes through a broker that MASKS
//! the real Rust panic behind a grpc/marshaling error. The extractor must skip
//! such masking lines and return the innermost app-code panic.

// The public API is wired into `triage::run`; the tests below exercise it.
#![allow(dead_code)]

use serde::Serialize;

/// The kind of failure we recognised in a service's logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CauseKind {
    /// A Rust `panicked at …` — the strongest, most specific signal.
    Panic,
    /// The process was killed with no panic message (OOM / SIGKILL).
    Killed,
    /// A non-panic fatal error (bind failure, connection refused, `os error`).
    Fatal,
    /// Logs ended without a recognised failure signature.
    Unknown,
}

/// A structured root cause extracted from a log stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RootCause {
    pub kind: CauseKind,
    /// `file:line:col` for a panic; `None` for a fatal / kill.
    pub location: Option<String>,
    /// The panic / error message (the captured field name lives here).
    pub message: String,
    /// A small trailing slice of the (ANSI-stripped) log for context.
    pub log_tail: String,
}

/// Extract the root cause from a service's log text.
///
/// Precedence: an app-code Rust panic (root, not the first masking line) >
/// a non-panic fatal (`os error`, address-in-use, connection refused) >
/// a bare kill (OOM / SIGKILL) > `None` (clean logs).
pub fn extract_root_cause(logs: &str) -> Option<RootCause> {
    let clean = strip_ansi(logs);
    let lines: Vec<&str> = clean.lines().collect();
    let tail = log_tail(&lines);

    // 1) Rust panics win. Collect them all, then pick the ROOT: prefer an
    //    app-code (`.rs`) location, and among those the LAST (innermost) — so a
    //    grpc/broker masking line that precedes the real panic never wins.
    let panics: Vec<Panic> = (0..lines.len())
        .filter_map(|i| parse_panic_at(&lines, i))
        .collect();
    if let Some(p) = choose_root_panic(&panics) {
        return Some(RootCause {
            kind: CauseKind::Panic,
            location: Some(p.location.clone()),
            message: p.message.clone(),
            log_tail: tail,
        });
    }

    // 2) A non-panic fatal (bind failure / refused / `os error`).
    if let Some(msg) = find_fatal(&lines) {
        return Some(RootCause {
            kind: CauseKind::Fatal,
            location: None,
            message: msg,
            log_tail: tail,
        });
    }

    // 3) A bare kill with no message (OOM / SIGKILL). Do NOT fabricate a panic.
    if let Some(msg) = find_kill(&lines) {
        return Some(RootCause {
            kind: CauseKind::Killed,
            location: None,
            message: msg,
            log_tail: tail,
        });
    }

    None
}

/// A single parsed `panicked at` occurrence.
struct Panic {
    location: String,
    message: String,
}

/// Parse a panic anchored at `lines[i]` if that line contains `panicked at`.
///
/// New-format panics read `… panicked at <file>:<line>:<col>:` with the message
/// either inline after the trailing colon or on the next non-empty line.
fn parse_panic_at(lines: &[&str], i: usize) -> Option<Panic> {
    let line = lines[i];
    let anchor = line.find("panicked at ")?;
    let rem = &line[anchor + "panicked at ".len()..];

    let loc_colon = find_location_end(rem)?;
    let location = rem[..loc_colon].trim().to_string();
    let inline = rem[loc_colon + 1..].trim();

    let message = if !inline.is_empty() {
        inline.to_string()
    } else {
        // The message is the next non-empty line (skip blank lines).
        lines
            .iter()
            .skip(i + 1)
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string()
    };

    Some(Panic { location, message })
}

/// Find the trailing colon of a `<file>:<line>:<col>:` location inside `rem`,
/// returning that colon's byte index. Purely structural — captures whatever the
/// path is, so a never-seen crate path is handled the same as a known one.
fn find_location_end(rem: &str) -> Option<usize> {
    let b = rem.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b':' {
            i += 1;
            continue;
        }
        // Expect :<digits>:<digits>: starting at i.
        let mut j = i + 1;
        let d1 = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == d1 || j >= b.len() || b[j] != b':' {
            i += 1;
            continue;
        }
        j += 1;
        let d2 = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == d2 || j >= b.len() || b[j] != b':' {
            i += 1;
            continue;
        }
        // b[j] is the trailing colon after the column number.
        return Some(j);
    }
    None
}

/// Choose the root panic: prefer app-code (`.rs`) locations, then the innermost
/// (last) — never the first, which may be a broker/CLI masking frame.
fn choose_root_panic(panics: &[Panic]) -> Option<&Panic> {
    panics
        .iter()
        .rev()
        .find(|p| p.location.contains(".rs"))
        .or_else(|| panics.last())
}

/// Match a non-panic fatal line (bind failure, refused, generic `os error`).
fn find_fatal(lines: &[&str]) -> Option<String> {
    const NEEDLES: [&str; 3] = ["Address already in use", "connection refused", "os error"];
    lines.iter().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        NEEDLES
            .iter()
            .any(|n| lower.contains(&n.to_ascii_lowercase()))
            .then(|| line.trim().to_string())
    })
}

/// Detect a bare process kill (OOM / SIGKILL) with no panic or fatal message.
fn find_kill(lines: &[&str]) -> Option<String> {
    const NEEDLES: [&str; 4] = ["killed", "sigkill", "signal: 9", "out of memory"];
    lines.iter().rev().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        NEEDLES
            .iter()
            .any(|n| lower.contains(n))
            .then(|| line.trim().to_string())
    })
}

/// Strip ANSI CSI escape sequences (`ESC [ … <final>`) from a string.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // ESC: consume a `[ … <final byte 0x40-0x7E>` CSI sequence.
        if chars.peek() == Some(&'[') {
            chars.next();
            for e in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&e) {
                    break;
                }
            }
        }
        // A lone ESC (or non-CSI) is simply dropped.
    }
    out
}

/// Keep a small trailing slice (last ~20 lines) of the log for context.
fn log_tail(lines: &[&str]) -> String {
    const TAIL: usize = 20;
    let start = lines.len().saturating_sub(TAIL);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERDE_MISSING: &str = include_str!("../../../tests/fixtures/helix_serde_missing_field.log");
    const PREGENESIS: &str = include_str!("../../../tests/fixtures/helix_pregenesis_unwrap.log");
    const INVENTED: &str = include_str!("../../../tests/fixtures/invented_field.log");
    const MULTI_MASKED: &str = include_str!("../../../tests/fixtures/multi_masked.log");
    const ANSI: &str = include_str!("../../../tests/fixtures/ansi_colored.log");
    const NEXT_LINE: &str = include_str!("../../../tests/fixtures/next_line_message.log");
    const OOM: &str = include_str!("../../../tests/fixtures/oom_killed.log");
    const BIND: &str = include_str!("../../../tests/fixtures/bind_error.log");
    const CLEAN: &str = include_str!("../../../tests/fixtures/clean.log");

    #[test]
    fn serde_missing_field_panic() {
        let rc = extract_root_cause(SERDE_MISSING).expect("should find a panic");
        assert_eq!(rc.kind, CauseKind::Panic);
        assert_eq!(
            rc.location.as_deref(),
            Some("/app/crates/common/src/config.rs:203:51")
        );
        // Field captured from the message, not string-matched.
        assert!(
            rc.message.contains("decoder"),
            "message should carry the missing field name: {}",
            rc.message
        );
    }

    #[test]
    fn pregenesis_unwrap_panic() {
        let rc = extract_root_cause(PREGENESIS).expect("should find a panic");
        assert_eq!(rc.kind, CauseKind::Panic);
        assert_eq!(
            rc.location.as_deref(),
            Some("crates/common/src/chain_info.rs:63:26")
        );
        assert!(
            rc.message.contains("unwrap()") && rc.message.contains("None"),
            "message should be the unwrap-on-None panic: {}",
            rc.message
        );
    }

    #[test]
    fn held_out_field_is_captured_not_matched() {
        // A field name we have never seen — proves pattern capture, not memory.
        let rc = extract_root_cause(INVENTED).expect("should find a panic");
        assert_eq!(rc.kind, CauseKind::Panic);
        assert!(
            rc.message.contains("foobar"),
            "held-out field name must be captured: {}",
            rc.message
        );
    }

    #[test]
    fn picks_root_panic_over_masking_line() {
        // The grpc/marshaling masking lines come FIRST; the real panic is later.
        let rc = extract_root_cause(MULTI_MASKED).expect("should find a panic");
        assert_eq!(rc.kind, CauseKind::Panic);
        assert_eq!(
            rc.location.as_deref(),
            Some("/app/crates/common/src/config.rs:203:51")
        );
        assert!(
            rc.message.contains("network_config"),
            "should return the ROOT panic message, not the masking line: {}",
            rc.message
        );
        // The masking noise must not leak into the message.
        assert!(
            !rc.message.contains("UTF-8"),
            "masking line must not be the reported cause: {}",
            rc.message
        );
    }

    #[test]
    fn strips_ansi_escape_codes() {
        let rc = extract_root_cause(ANSI).expect("should find a panic");
        assert_eq!(rc.kind, CauseKind::Panic);
        assert_eq!(rc.location.as_deref(), Some("src/main.rs:10:5"));
        assert!(
            !rc.message.contains('\u{1b}'),
            "ANSI escape must be stripped from the message: {:?}",
            rc.message
        );
        assert!(
            rc.message.contains("explicit panic"),
            "message should survive ANSI stripping: {}",
            rc.message
        );
    }

    #[test]
    fn panic_message_on_next_line() {
        let rc = extract_root_cause(NEXT_LINE).expect("should find a panic");
        assert_eq!(rc.kind, CauseKind::Panic);
        assert_eq!(rc.location.as_deref(), Some("src/worker.rs:88:12"));
        assert!(
            rc.message.contains("something went terribly wrong"),
            "next-line message should be captured: {}",
            rc.message
        );
    }

    #[test]
    fn oom_returns_killed_not_a_fake_panic() {
        let rc = extract_root_cause(OOM).expect("should report the kill");
        assert_eq!(rc.kind, CauseKind::Killed);
        assert!(rc.location.is_none(), "a kill has no source location");
    }

    #[test]
    fn non_rust_fatal_is_matched() {
        let rc = extract_root_cause(BIND).expect("should find a fatal");
        assert_eq!(rc.kind, CauseKind::Fatal);
        assert!(
            rc.message.contains("os error 98") || rc.message.contains("Address already in use"),
            "fatal message should name the bind error: {}",
            rc.message
        );
    }

    #[test]
    fn clean_logs_return_none() {
        assert!(extract_root_cause(CLEAN).is_none());
    }

    #[test]
    fn log_tail_is_populated_for_a_cause() {
        let rc = extract_root_cause(SERDE_MISSING).expect("panic");
        assert!(!rc.log_tail.is_empty(), "log_tail should carry context");
    }
}
