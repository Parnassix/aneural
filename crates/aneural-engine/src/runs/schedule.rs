//! When a script is next owed, from a cadence a human wrote down.
//!
//! The vocabulary is not invented here. It is the one the motivating project
//! already keeps in a markdown table and already parses with its own script, so
//! Aneural reads the same words and applies the same thresholds rather than
//! competing with it. `cadences` in config extends the table for a project that
//! uses a word this does not know.

use std::collections::BTreeMap;
use time::Date;

/// Past its due date at all.
pub const DUE_THRESHOLD_DAYS: i64 = 0;
/// More than a week past it.
pub const OVERDUE_THRESHOLD_DAYS: i64 = 7;

/// Where a schedule stands right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    /// Refreshed recently enough.
    Fresh,
    /// Past its due date.
    Due,
    /// More than [`OVERDUE_THRESHOLD_DAYS`] past it.
    Overdue,
    /// A multi-week baseline is still being built, so "due" is meaningless —
    /// the work is happening, it just has no finish date yet.
    Building,
    /// A cadence that deliberately never comes due: frozen, on-demand, none.
    Never,
    /// No cadence, or one nothing recognises, or no readable last-refresh date.
    Unknown,
}

impl Due {
    pub fn label(self) -> &'static str {
        match self {
            Due::Fresh => "fresh",
            Due::Due => "due",
            Due::Overdue => "overdue",
            Due::Building => "building",
            Due::Never => "never",
            Due::Unknown => "unknown",
        }
    }

    /// Whether this is worth the reader's attention.
    pub fn wants_attention(self) -> bool {
        matches!(self, Due::Due | Due::Overdue)
    }
}

/// How many days a cadence allows before the next refresh is owed.
///
/// `None` means it never comes due, which is different from not knowing: a
/// frozen source is correct to sit untouched forever.
pub fn interval_days(cadence: &str, extra: &BTreeMap<String, u32>) -> Option<Option<u32>> {
    let full = cadence.trim().to_ascii_lowercase();
    // The whole cell first, so a project can map a phrase it writes verbatim.
    if let Some(days) = extra.get(&full) {
        return Some(Some(*days));
    }
    // Then the leading word, because a cadence cell carries commentary the same
    // way a last-refreshed cell does: `monthly (POC data on disk; not formally
    // onboarded)` is a monthly source with a note attached, and reading it as
    // unrecognised loses the cadence over the parenthesis.
    let key = full.split_whitespace().next().unwrap_or_default();
    if let Some(days) = extra.get(key) {
        return Some(Some(*days));
    }
    let days = match key {
        "frozen" | "on-demand" | "none" => return Some(None),
        "daily" => 1,
        "" => return None,
        // A day-named weekly is still weekly; which day it lands on decides the
        // hour it fires, not how long it may go unrefreshed.
        k if k.starts_with("weekly") => 7,
        "monthly" => 30,
        "quarterly" => 90,
        "semi-annual" => 180,
        // A live-rolling API has no edition to wait for, so it is re-pulled on a
        // week's rhythm rather than never.
        "continuous" => 7,
        k if k.starts_with("annual") => 365,
        _ => return None,
    };
    Some(Some(days))
}

/// The date a `last_refreshed` cell reports, if it reports one.
///
/// Real cells are a date followed by however much commentary the author needed:
/// `2026-05-12 (audit baseline + sample-validation …; PID 63039 → log …)`. The
/// leading token is the claim; the rest is prose.
pub fn last_date(cell: &str) -> Option<Date> {
    let head = cell.split_whitespace().next()?;
    let mut parts = head.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    let d: u8 = parts.next()?.parse().ok()?;
    Date::from_calendar_date(y, time::Month::try_from(m).ok()?, d).ok()
}

/// A baseline still being built, rather than a date.
///
/// Written as an `in-progress` sentinel in place of the date for a multi-week
/// first pass. Treating it as "no date" would report it overdue forever.
pub fn is_building(cell: &str) -> bool {
    cell.trim().to_ascii_lowercase().starts_with("in-progress")
}

/// Where a declaration stands, given what it asked for and when it last ran.
pub fn status(
    cadence: &str,
    last_refreshed: &str,
    now: Date,
    extra: &BTreeMap<String, u32>,
) -> Due {
    let Some(interval) = interval_days(cadence, extra) else {
        return Due::Unknown;
    };
    let Some(days) = interval else {
        return Due::Never;
    };
    if is_building(last_refreshed) {
        return Due::Building;
    }
    let Some(last) = last_date(last_refreshed) else {
        return Due::Unknown;
    };
    let late = (now - last).whole_days() - i64::from(days);
    match late {
        l if l > OVERDUE_THRESHOLD_DAYS => Due::Overdue,
        l if l > DUE_THRESHOLD_DAYS => Due::Due,
        _ => Due::Fresh,
    }
}

/// The date the next refresh is owed, for display beside a status.
pub fn next_due(
    last_refreshed: &str,
    cadence: &str,
    extra: &BTreeMap<String, u32>,
) -> Option<Date> {
    let days = interval_days(cadence, extra)??;
    let last = last_date(last_refreshed)?;
    last.checked_add(time::Duration::days(days.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u8, d: u8) -> Date {
        Date::from_calendar_date(y, time::Month::try_from(m).unwrap(), d).unwrap()
    }

    fn no_extra() -> BTreeMap<String, u32> {
        BTreeMap::new()
    }

    #[test]
    fn the_cadence_words_the_project_already_uses_are_the_ones_understood() {
        let e = no_extra();
        // every cadence appearing in the real table, with its own counts
        for (word, days) in [
            ("daily", 1),
            ("weekly-mon", 7),
            ("weekly-tue", 7),
            ("weekly-rolling", 7),
            ("monthly", 30),
            ("quarterly", 90),
            ("semi-annual", 180),
            ("continuous", 7),
            ("annual-jan", 365),
            ("annual-eoy", 365),
            ("annual-winter", 365),
        ] {
            assert_eq!(interval_days(word, &e), Some(Some(days)), "{word}");
        }
        // deliberately never due, which is not the same as unrecognised
        for word in ["frozen", "on-demand", "none"] {
            assert_eq!(interval_days(word, &e), Some(None), "{word}");
        }
        assert_eq!(interval_days("whenever", &e), None);
    }

    #[test]
    fn a_cadence_word_survives_the_commentary_written_after_it() {
        // Verbatim from the real table, three rows of it.
        let e = no_extra();
        assert_eq!(
            interval_days("monthly (POC data on disk; not formally onboarded)", &e),
            Some(Some(30))
        );
        assert_eq!(
            status(
                "monthly (POC data on disk; not formally onboarded)",
                "2026-06-07",
                day(2026, 9, 27),
                &e
            ),
            Due::Overdue
        );
        // and a hyphenated word is still one word, not two
        assert_eq!(interval_days("weekly-mon (see notes)", &e), Some(Some(7)));
        assert_eq!(interval_days("on-demand — never pulled", &e), Some(None));
    }

    #[test]
    fn a_project_can_teach_it_a_word_and_override_one() {
        let extra = BTreeMap::from([("fortnightly".to_string(), 14), ("daily".to_string(), 2)]);
        assert_eq!(interval_days("fortnightly", &extra), Some(Some(14)));
        assert_eq!(interval_days("daily", &extra), Some(Some(2)), "config wins");
    }

    #[test]
    fn a_date_is_read_out_of_however_much_prose_follows_it() {
        // verbatim shape from the real table
        let cell = "2026-05-12 (audit baseline + sample-validation \
                    `dm_spl_daily_update_05112026.zip` (264.7 MB); full ~70 GB bootstrap \
                    launched same day, PID 63039 → log `logs/dailymed_20260512.log`)";
        assert_eq!(last_date(cell), Some(day(2026, 5, 12)));
        assert_eq!(last_date("2026-06-07"), Some(day(2026, 6, 7)));
        assert_eq!(last_date("—"), None);
        assert_eq!(last_date("— (ARTG/DAEN not downloaded)"), None);
    }

    #[test]
    fn a_baseline_still_building_is_never_reported_overdue() {
        // Also verbatim: the cell says `in-progress` where a date would go, for
        // a first pass measured in weeks. Reading that as "no date" would put
        // the row permanently in the red.
        let cell = "in-progress (v1 baseline build started 2026-05-02; ETA ~2026-06-08 …)";
        assert!(is_building(cell));
        assert_eq!(
            status("weekly-mon", cell, day(2026, 9, 27), &no_extra()),
            Due::Building
        );
        assert!(!Due::Building.wants_attention());
    }

    #[test]
    fn a_daily_source_is_due_the_day_after_and_overdue_a_week_later() {
        let e = no_extra();
        let last = "2026-09-20";
        assert_eq!(status("daily", last, day(2026, 9, 20), &e), Due::Fresh);
        assert_eq!(status("daily", last, day(2026, 9, 21), &e), Due::Fresh);
        assert_eq!(status("daily", last, day(2026, 9, 22), &e), Due::Due);
        assert_eq!(status("daily", last, day(2026, 9, 28), &e), Due::Due);
        assert_eq!(status("daily", last, day(2026, 9, 29), &e), Due::Overdue);
        assert!(Due::Overdue.wants_attention());
    }

    #[test]
    fn a_frozen_source_is_never_due_however_long_it_sits() {
        let e = no_extra();
        assert_eq!(
            status("frozen", "2019-01-01", day(2026, 9, 27), &e),
            Due::Never
        );
        assert_eq!(
            status("on-demand", "— (not downloaded)", day(2026, 9, 27), &e),
            Due::Never,
            "and it does not need a date to say so"
        );
    }

    #[test]
    fn an_unreadable_cadence_says_so_rather_than_guessing() {
        let e = no_extra();
        assert_eq!(status("", "2026-09-20", day(2026, 9, 27), &e), Due::Unknown);
        assert_eq!(
            status("daily", "sometime last spring", day(2026, 9, 27), &e),
            Due::Unknown
        );
    }

    #[test]
    fn the_next_date_owed_is_the_last_plus_the_interval() {
        let e = no_extra();
        assert_eq!(
            next_due("2026-09-20", "weekly-mon", &e),
            Some(day(2026, 9, 27))
        );
        assert_eq!(next_due("2026-09-20", "frozen", &e), None);
        assert_eq!(next_due("in-progress (…)", "daily", &e), None);
    }
}
