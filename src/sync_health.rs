//! Per-account sync health (#0071).
//!
//! A sync that fails used to leave one transient status line and nothing else.
//! In a multi-account run that line loses the race against the accounts that
//! succeeded: #0068 found `perso` failing at IMAP login on every tick for seven
//! weeks while `tum` and `assistant`, fifteen seconds slower, overwrote the
//! line with `Fetch complete` each time.
//!
//! The fix is to stop expressing the outcome as a status line at all. Each
//! account carries its own [`SyncHealth`], updated when its own result lands,
//! so a failure survives every later success of a *different* account and stays
//! on screen until that same account syncs cleanly.
//!
//! The state is per session and deliberately not persisted: the question it
//! answers is "is this account working right now", and a mark restored from
//! disk before the first sync of a new run would answer a stale one.

use chrono::{DateTime, Local};

/// How much of an error message the surfaces keep.
///
/// Sized to the sidebar block that renders it: two rows of roughly forty
/// columns. That is enough for the whole of the message this ticket exists
/// for, `IMAP login failed: no response: code: None, info: Some("no such
/// user")`, whose discriminating half is at the end.
const MAX_REASON_CHARS: usize = 80;

/// The first line of an error, trimmed and capped at [`MAX_REASON_CHARS`].
///
/// `anyhow` chains print their context on one line but the underlying IO or
/// TLS errors sometimes carry embedded newlines; a multi-line reason would
/// break every single-line surface, so only the first line survives.
pub fn short_reason(error: &str) -> String {
    let first = error.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let first = first.trim();
    if first.chars().count() <= MAX_REASON_CHARS {
        return first.to_string();
    }
    let head: String = first.chars().take(MAX_REASON_CHARS - 1).collect();
    format!("{head}\u{2026}")
}

/// The outcome of an account's last completed sync.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SyncHealth {
    /// No sync has completed for this account since the session started.
    #[default]
    Unknown,
    Ok {
        at: DateTime<Local>,
    },
    Failed {
        /// [`short_reason`] of the error the sync returned.
        reason: String,
        at: DateTime<Local>,
        /// Failures in a row, i.e. how long this has been broken. An auth
        /// failure standing for weeks is a different message from one tick
        /// that timed out; the surfaces say so by showing the count once it
        /// is above one.
        consecutive: u32,
    },
}

impl SyncHealth {
    /// This health after one more sync finished, `Ok(())` or `Err(message)`.
    ///
    /// A success clears the failure outright: the account is working again and
    /// the mark has done its job. A failure keeps counting from the previous
    /// one rather than restarting, which is the only way the surfaces can tell
    /// a hiccup from an outage.
    pub fn updated(&self, outcome: Result<(), &str>, at: DateTime<Local>) -> Self {
        match outcome {
            Ok(()) => Self::Ok { at },
            Err(error) => Self::Failed {
                reason: short_reason(error),
                at,
                consecutive: match self {
                    Self::Failed { consecutive, .. } => consecutive.saturating_add(1),
                    _ => 1,
                },
            },
        }
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    /// The two lines the sidebar draws for a failure: a headline carrying the
    /// marker, the repeat count and the time, then the reason on its own.
    /// `None` when the last sync worked or none has run: a healthy account
    /// says nothing.
    ///
    /// Two lines rather than one because the sidebar is about forty columns
    /// wide, and a single line spends most of them on the preamble and then
    /// truncates the reason away, which is the only part that says what to do.
    pub fn failure_lines(&self) -> Option<(String, String)> {
        match self {
            Self::Failed { reason, at, consecutive } => {
                let time = at.format("%H:%M");
                let repeat = if *consecutive > 1 {
                    format!(" x{consecutive}")
                } else {
                    String::new()
                };
                Some((
                    format!("\u{26a0} sync failed{repeat} {time}"),
                    reason.clone(),
                ))
            }
            _ => None,
        }
    }
}

/// The closing line of a `mp sync` run, naming every account that failed.
///
/// `failed` is every account whose sync returned an error, in the order they
/// were synced. `None` when the run was clean: a summary that always prints
/// trains the reader to skip it.
///
/// Named accounts, not a count: #0068's whole cost was that the failure was
/// anonymous.
pub fn failure_summary(total: usize, failed: &[String]) -> Option<String> {
    if failed.is_empty() {
        return None;
    }
    Some(format!(
        "{} of {} account(s) failed to sync: {}",
        failed.len(),
        total,
        failed.join(", ")
    ))
}

/// The process exit code for a `mp sync` run in which `failed` accounts failed.
///
/// One code for any failure, partial or total, and the same code whether one
/// account was named or `--all-accounts` was passed. A caller writes
/// `mp sync --all-accounts || alert`, and a partial failure that exited 0 is
/// exactly the silence this ticket exists to remove. A distinct code for
/// "some but not all" would only be readable by a caller that already knows
/// how many accounts are configured.
pub fn exit_code(failed: &[String]) -> i32 {
    if failed.is_empty() {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 8, 6, hour, minute, 0)
            .single()
            .expect("unambiguous local time")
    }

    // -----------------------------------------------------------------------
    // short_reason
    // -----------------------------------------------------------------------

    #[test]
    fn short_reason_keeps_the_first_non_empty_line() {
        assert_eq!(
            short_reason("IMAP login failed: no such user"),
            "IMAP login failed: no such user"
        );
        assert_eq!(
            short_reason("\n  IMAP login failed: no such user\ncaused by: io error"),
            "IMAP login failed: no such user"
        );
        assert_eq!(short_reason(""), "");
    }

    #[test]
    fn short_reason_caps_a_long_error_with_an_ellipsis() {
        let long = "x".repeat(200);
        let short = short_reason(&long);
        assert_eq!(short.chars().count(), MAX_REASON_CHARS);
        assert!(short.ends_with('\u{2026}'));
    }

    /// The cap counts characters, so a multi-byte error cannot be cut mid-char
    /// (which would panic on a byte slice).
    #[test]
    fn short_reason_counts_characters_not_bytes() {
        let long = "ü".repeat(200);
        assert_eq!(short_reason(&long).chars().count(), MAX_REASON_CHARS);
    }

    // -----------------------------------------------------------------------
    // SyncHealth::updated
    // -----------------------------------------------------------------------

    #[test]
    fn a_first_failure_records_the_reason_the_time_and_one_occurrence() {
        let health = SyncHealth::default().updated(Err("IMAP login failed: no such user"), at(15, 42));
        assert_eq!(
            health,
            SyncHealth::Failed {
                reason: "IMAP login failed: no such user".to_string(),
                at: at(15, 42),
                consecutive: 1,
            }
        );
        assert!(health.is_failed());
    }

    /// The distinction the ticket asks for between a hiccup and an outage: the
    /// count keeps rising while the same account keeps failing.
    #[test]
    fn repeated_failures_accumulate() {
        let mut health = SyncHealth::default();
        for _ in 0..3 {
            health = health.updated(Err("IMAP login failed: no such user"), at(15, 42));
        }
        let SyncHealth::Failed { consecutive, .. } = health else {
            panic!("three failures in a row leave a failed health");
        };
        assert_eq!(consecutive, 3);
    }

    /// A mark is cleared by that account syncing cleanly, and only by that.
    #[test]
    fn a_success_clears_the_failure() {
        let health = SyncHealth::default()
            .updated(Err("IMAP login failed"), at(15, 42))
            .updated(Ok(()), at(15, 43));
        assert_eq!(health, SyncHealth::Ok { at: at(15, 43) });
        assert!(!health.is_failed());
        assert_eq!(health.failure_lines(), None);
    }

    // -----------------------------------------------------------------------
    // failure_lines
    // -----------------------------------------------------------------------

    #[test]
    fn the_failure_lines_carry_the_time_and_the_reason() {
        let health = SyncHealth::default().updated(Err("IMAP login failed: no such user"), at(15, 42));
        assert_eq!(
            health.failure_lines().unwrap(),
            (
                "\u{26a0} sync failed 15:42".to_string(),
                "IMAP login failed: no such user".to_string(),
            )
        );
    }

    #[test]
    fn the_failure_headline_shows_the_repeat_count_only_once_it_is_above_one() {
        let mut health = SyncHealth::default().updated(Err("nope"), at(15, 42));
        assert_eq!(health.failure_lines().unwrap().0, "\u{26a0} sync failed 15:42");
        health = health.updated(Err("nope"), at(15, 43));
        assert_eq!(
            health.failure_lines().unwrap().0,
            "\u{26a0} sync failed x2 15:43"
        );
    }

    #[test]
    fn a_healthy_or_unknown_account_has_no_failure_lines() {
        assert_eq!(SyncHealth::Unknown.failure_lines(), None);
        assert_eq!(SyncHealth::Ok { at: at(9, 0) }.failure_lines(), None);
    }

    // -----------------------------------------------------------------------
    // failure_summary / exit_code
    // -----------------------------------------------------------------------

    #[test]
    fn a_clean_run_has_no_summary_and_exits_zero() {
        assert_eq!(failure_summary(3, &[]), None);
        assert_eq!(exit_code(&[]), 0);
    }

    /// The acceptance criterion for the CLI half: the failing account is named,
    /// not counted.
    #[test]
    fn a_partial_failure_names_the_accounts_and_exits_one() {
        let failed = vec!["perso".to_string()];
        assert_eq!(
            failure_summary(3, &failed).unwrap(),
            "1 of 3 account(s) failed to sync: perso"
        );
        assert_eq!(exit_code(&failed), 1);
    }

    /// A total failure is the same code as a partial one: `mp sync || alert`
    /// must fire either way.
    #[test]
    fn a_total_failure_exits_one_as_well() {
        let failed = vec!["perso".to_string(), "tum".to_string()];
        assert_eq!(
            failure_summary(2, &failed).unwrap(),
            "2 of 2 account(s) failed to sync: perso, tum"
        );
        assert_eq!(exit_code(&failed), 1);
    }
}
