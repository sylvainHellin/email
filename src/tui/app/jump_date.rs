//! The date grammar behind jump-to-date (`g d`, #0017).
//!
//! A pure function over a string and "today", so the whole grammar is tested
//! without a terminal and without a clock: [`parse_jump_date`] never reads
//! `now()` itself, the caller passes the day it means.
//!
//! The grammar is deliberately small and closed. It answers the two questions
//! a large archive actually gets navigated by -- an absolute point
//! (`2024-03-07`, `2024-03`, `2024`) and a rough distance back (`last week`,
//! `3 months ago`, `yesterday`) -- and refuses everything else with a message
//! naming what it does accept. A natural-language date parser was the obvious
//! next step and was left out on purpose: it is a dependency and an ambiguity
//! budget for a key that moves a cursor, and a wrong guess here silently lands
//! the user in the wrong year.

use chrono::{Months, NaiveDate};

/// The forms [`parse_jump_date`] accepts, as one line for a status message.
pub const JUMP_DATE_HELP: &str =
    "YYYY-MM-DD, YYYY-MM, YYYY, today, yesterday, last week/month/year, or 'N days/weeks/months/years ago'";

/// Resolve a jump-to-date input against `today`.
///
/// Returns the target *day*; the caller decides what "jump to that day" means
/// for its list. `Err` carries a one-line reason for the status bar.
pub fn parse_jump_date(input: &str, today: NaiveDate) -> Result<NaiveDate, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(format!("Jump to date: type one of {JUMP_DATE_HELP}"));
    }
    let lower = raw.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();

    // Absolute: the three prefixes of an ISO date, each meaning its first day.
    // `2024` is the start of that year, which is where a jump into an archive
    // wants to land: the newest message *on or before* it is the last of 2024
    // in a newest-first list, and stepping back from there is `k`.
    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Ok(date);
    }
    if let Some(date) = parse_year_month(raw) {
        return Ok(date);
    }
    if let Some(date) = parse_year(raw) {
        return Ok(date);
    }

    match words.as_slice() {
        ["today"] | ["now"] => return Ok(today),
        ["yesterday"] => return sub(today, 1, Unit::Day),
        ["last", unit] | ["a", unit, "ago"] | ["one", unit, "ago"] => {
            if let Some(unit) = Unit::parse(unit) {
                return sub(today, 1, unit);
            }
        }
        [n, unit, "ago"] => {
            if let (Ok(n), Some(unit)) = (n.parse::<u32>(), Unit::parse(unit)) {
                return sub(today, n, unit);
            }
        }
        _ => {}
    }

    Err(format!("Cannot read '{raw}' as a date. Try {JUMP_DATE_HELP}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Day,
    Week,
    Month,
    Year,
}

impl Unit {
    /// Singular and plural both, because a user types whichever reads right
    /// (`1 day ago`, `3 days ago`) and neither is a different meaning.
    fn parse(word: &str) -> Option<Self> {
        match word {
            "day" | "days" => Some(Self::Day),
            "week" | "weeks" => Some(Self::Week),
            "month" | "months" => Some(Self::Month),
            "year" | "years" => Some(Self::Year),
            _ => None,
        }
    }
}

/// `today` minus `n` units, saturating at nothing: an out-of-range result is
/// an error rather than a clamp, because a silent clamp to year 0 would be a
/// jump the user did not ask for.
fn sub(today: NaiveDate, n: u32, unit: Unit) -> Result<NaiveDate, String> {
    let out = match unit {
        Unit::Day => today.checked_sub_days(chrono::Days::new(n as u64)),
        Unit::Week => today.checked_sub_days(chrono::Days::new(n as u64 * 7)),
        Unit::Month => today.checked_sub_months(Months::new(n)),
        Unit::Year => today.checked_sub_months(Months::new(n.saturating_mul(12))),
    };
    out.ok_or_else(|| "That date is out of range".to_string())
}

fn parse_year_month(raw: &str) -> Option<NaiveDate> {
    let (year, month) = raw.split_once('-')?;
    let year: i32 = parse_year_digits(year)?;
    if month.len() != 2 {
        return None;
    }
    let month: u32 = month.parse().ok()?;
    NaiveDate::from_ymd_opt(year, month, 1)
}

fn parse_year(raw: &str) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(parse_year_digits(raw)?, 1, 1)
}

/// Exactly four digits: `24` is not a year here, because guessing whether it
/// means 1924, 2024 or the 24th of something is the ambiguity this grammar
/// refuses to have.
fn parse_year_digits(s: &str) -> Option<i32> {
    if s.len() != 4 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// The day a list row falls on, from its `date_sort` key
/// (`YYYY-MM-DDTHH:MM:SS`, UTC; see `types::resolve_date`).
///
/// `None` for a row with no usable date, which the jump skips rather than
/// treats as the epoch.
pub fn day_of_sort_key(date_sort: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(date_sort.get(..10)?, "%Y-%m-%d").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, 11).unwrap()
    }

    fn parse(s: &str) -> NaiveDate {
        parse_jump_date(s, today()).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn absolute_forms_are_their_own_first_day() {
        assert_eq!(parse("2024-03-07"), NaiveDate::from_ymd_opt(2024, 3, 7).unwrap());
        assert_eq!(parse("2024-03"), NaiveDate::from_ymd_opt(2024, 3, 1).unwrap());
        assert_eq!(parse("2024"), NaiveDate::from_ymd_opt(2024, 1, 1).unwrap());
        assert_eq!(parse("  2024-03-07 "), NaiveDate::from_ymd_opt(2024, 3, 7).unwrap());
    }

    #[test]
    fn relative_forms_count_back_from_today() {
        assert_eq!(parse("today"), today());
        assert_eq!(parse("yesterday"), NaiveDate::from_ymd_opt(2026, 8, 10).unwrap());
        assert_eq!(parse("last week"), NaiveDate::from_ymd_opt(2026, 8, 4).unwrap());
        assert_eq!(parse("2 weeks ago"), NaiveDate::from_ymd_opt(2026, 7, 28).unwrap());
        assert_eq!(parse("3 days ago"), NaiveDate::from_ymd_opt(2026, 8, 8).unwrap());
        assert_eq!(parse("2 months ago"), NaiveDate::from_ymd_opt(2026, 6, 11).unwrap());
        assert_eq!(parse("last month"), NaiveDate::from_ymd_opt(2026, 7, 11).unwrap());
        assert_eq!(parse("1 year ago"), NaiveDate::from_ymd_opt(2025, 8, 11).unwrap());
        assert_eq!(parse("last year"), NaiveDate::from_ymd_opt(2025, 8, 11).unwrap());
        assert_eq!(parse("a week ago"), parse("last week"));
    }

    /// Case and spacing are the user's, not the grammar's.
    #[test]
    fn input_is_case_and_whitespace_insensitive() {
        assert_eq!(parse("Last  Week"), parse("last week"));
        assert_eq!(parse("TODAY"), today());
    }

    /// A month subtraction that would leave an impossible day is clamped by
    /// chrono to the last day of the target month, which is the only sane
    /// answer and is pinned here so a chrono change cannot move it quietly.
    #[test]
    fn a_month_back_from_the_31st_lands_on_the_last_day_of_that_month() {
        let end = NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        assert_eq!(
            parse_jump_date("1 month ago", end).unwrap(),
            NaiveDate::from_ymd_opt(2026, 2, 28).unwrap()
        );
    }

    #[test]
    fn nonsense_is_refused_with_the_accepted_forms_named() {
        for input in ["", "   ", "tomorrow", "24", "2024-13-01", "next week", "3 fortnights ago", "-1 days ago"] {
            let err = parse_jump_date(input, today()).unwrap_err();
            assert!(err.contains("YYYY-MM-DD"), "{input}: {err}");
        }
    }

    #[test]
    fn a_row_day_comes_off_the_sort_key() {
        assert_eq!(
            day_of_sort_key("2026-07-30T09:00:00"),
            Some(NaiveDate::from_ymd_opt(2026, 7, 30).unwrap())
        );
        assert_eq!(day_of_sort_key(""), None);
        assert_eq!(day_of_sort_key("not-a-date"), None);
    }
}
