//! Rounding a day's raw durations to whole increments, and keeping a day
//! under its cap. Everything here is integer arithmetic: raw time is in
//! microseconds, rounded time in whole increments, so a displayed total is
//! always exactly the sum of what is displayed.

use std::cmp::Ordering;

use super::model::{Adjustment, Rounding, TimesheetEntry};

const MICROS: u64 = 1_000_000;

/// One engagement's raw time on one day.
#[derive(Clone, Debug)]
pub(crate) struct RawEntry {
    pub(crate) raw_micros: u64,
    /// Tie-break: a lower key wins a tie, so rounding never depends on the
    /// order entries arrive in.
    pub(crate) key: String,
}

/// What rounding decided for one entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Rounded {
    pub(crate) units: u64,
    /// Raised to the minimum entry.
    pub(crate) raised: bool,
    /// Balancing the day gave this entry a different number of increments
    /// than rounding it alone would have.
    pub(crate) balanced: bool,
    /// Removed: below `drop_below`, or rounded to nothing.
    pub(crate) dropped: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RoundParams {
    pub(crate) increment_seconds: u64,
    pub(crate) rounding: Rounding,
    pub(crate) min_entry_seconds: u64,
    pub(crate) drop_below_seconds: u64,
}

/// Rounds one day's entries.
///
/// 1. Each raw value becomes whole increments (`balanced` rounds the day's
///    total and shares the leftover increments out by largest remainder, so
///    every entry stays within one increment of its raw value and the day
///    within half an increment of its raw total).
/// 2. An entry whose raw time is below `drop_below` is dropped. Otherwise one
///    under `min_entry` is raised to it, and one that rounded to nothing is
///    dropped. A dropped entry is reported by the caller, never lost silently.
pub(crate) fn round_day(entries: &[RawEntry], params: &RoundParams) -> Vec<Rounded> {
    let increment = params.increment_seconds.max(1) * MICROS;
    let mut results: Vec<Rounded> = match params.rounding {
        Rounding::Nearest => entries
            .iter()
            .map(|entry| units(entry.raw_micros + increment / 2, increment))
            .collect(),
        Rounding::Up => entries
            .iter()
            .map(|entry| units(entry.raw_micros + increment - 1, increment))
            .collect(),
        Rounding::Down => entries
            .iter()
            .map(|entry| units(entry.raw_micros, increment))
            .collect(),
        Rounding::Balanced => balance(entries, increment),
    };
    let minimum_units = params
        .min_entry_seconds
        .div_ceil(params.increment_seconds.max(1));
    for (entry, result) in entries.iter().zip(&mut results) {
        if entry.raw_micros < params.drop_below_seconds * MICROS {
            *result = Rounded {
                dropped: true,
                ..Rounded::default()
            };
        } else if result.units * params.increment_seconds.max(1) < params.min_entry_seconds {
            result.units = minimum_units;
            result.raised = true;
            result.balanced = false;
        } else if result.units == 0 {
            result.dropped = true;
        }
    }
    results
}

fn units(numerator: u64, increment: u64) -> Rounded {
    Rounded {
        units: numerator / increment,
        ..Rounded::default()
    }
}

fn balance(entries: &[RawEntry], increment: u64) -> Vec<Rounded> {
    let total: u64 = entries.iter().map(|entry| entry.raw_micros).sum();
    let target = (total + increment / 2) / increment;
    let mut results: Vec<Rounded> = entries
        .iter()
        .map(|entry| units(entry.raw_micros, increment))
        .collect();
    let floors: u64 = results.iter().map(|result| result.units).sum();
    // The target is at least the sum of the floors, and at most one more per
    // entry that has a remainder, so every leftover increment has a home.
    let leftover = target.saturating_sub(floors) as usize;
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&left, &right| {
        let remainder = |index: usize| entries[index].raw_micros % increment;
        remainder(right)
            .cmp(&remainder(left))
            .then(entries[right].raw_micros.cmp(&entries[left].raw_micros))
            .then_with(|| entries[left].key.cmp(&entries[right].key))
    });
    for &index in order.iter().take(leftover) {
        results[index].units += 1;
    }
    for (entry, result) in entries.iter().zip(&mut results) {
        result.balanced = result.units != (entry.raw_micros + increment / 2) / increment;
    }
    results
}

/// Brings one day's entries under `cap_seconds`.
///
/// Manual entries and overrides are subtracted first and are never reduced.
/// While the estimates exceed what is left, one increment comes off the entry
/// that was rounded up the most (ties: non-billable first, then the smaller
/// raw value, then the key), and that entry is marked `Capped`. The cap is
/// applied after rounding, so every estimate stays a whole number of
/// increments. Re-runnable: the ledger calls it again once manual entries and
/// overrides are known.
///
/// Returns a warning when manual hours and overrides alone exceed the cap.
pub(crate) fn enforce_daily_cap(
    entries: &mut [TimesheetEntry],
    increment_seconds: u64,
    cap_seconds: u64,
) -> Option<String> {
    let increment = increment_seconds.max(1);
    let reserved: u64 = entries
        .iter()
        .map(|entry| entry.manual_seconds + entry.override_seconds.unwrap_or(0))
        .sum();
    let available = cap_seconds.saturating_sub(reserved);
    loop {
        let estimated: u64 = entries
            .iter()
            .filter(|entry| entry.override_seconds.is_none())
            .map(|entry| entry.estimated_seconds)
            .sum();
        if estimated <= available {
            break;
        }
        let chosen = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.override_seconds.is_none() && entry.estimated_seconds >= increment
            })
            .max_by(|(_, left), (_, right)| compare_for_capping(left, right))
            .map(|(index, _)| index);
        let Some(index) = chosen else { break };
        let entry = &mut entries[index];
        entry.estimated_seconds -= increment;
        if !entry.adjustments.contains(&Adjustment::Capped) {
            entry.adjustments.push(Adjustment::Capped);
        }
    }
    (reserved > cap_seconds).then(|| {
        let date = entries.first().map(|entry| entry.date.to_string());
        format!(
            "{}: manual hours and overrides alone ({}) exceed the daily cap ({}); nothing was removed from them",
            date.unwrap_or_default(),
            clock(reserved),
            clock(cap_seconds)
        )
    })
}

/// `Greater` means "cap this one first".
fn compare_for_capping(left: &TimesheetEntry, right: &TimesheetEntry) -> Ordering {
    let rounded_up = |entry: &TimesheetEntry| {
        entry.estimated_seconds as i128 * MICROS as i128 - (entry.raw_seconds * 1e6).round() as i128
    };
    rounded_up(left)
        .cmp(&rounded_up(right))
        // Non-billable work goes first: `false` must compare greater.
        .then(right.billable.cmp(&left.billable))
        // Then the smaller raw value (the cheaper hour to lose).
        .then(right.raw_seconds.total_cmp(&left.raw_seconds))
        // Then the lower key.
        .then_with(|| (&right.engagement, &right.detail).cmp(&(&left.engagement, &left.detail)))
}

fn clock(seconds: u64) -> String {
    format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;
    use crate::timesheet::model::{EntryStatus, Evidence};

    const INC: u64 = 900;

    fn params(rounding: Rounding) -> RoundParams {
        RoundParams {
            increment_seconds: INC,
            rounding,
            min_entry_seconds: 0,
            drop_below_seconds: 0,
        }
    }

    fn raw(seconds: &[u64]) -> Vec<RawEntry> {
        seconds
            .iter()
            .enumerate()
            .map(|(index, seconds)| RawEntry {
                raw_micros: seconds * MICROS + 123,
                key: format!("e{index}"),
            })
            .collect()
    }

    /// A small deterministic generator, so the property checks below are
    /// loops over many shapes without a dependency.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % bound
        }
    }

    fn random_day(random: &mut Lcg) -> Vec<RawEntry> {
        let count = 1 + random.next(6) as usize;
        (0..count)
            .map(|index| RawEntry {
                raw_micros: 1 + random.next(6 * 3600 * MICROS),
                key: format!("e{index}"),
            })
            .collect()
    }

    #[test]
    fn nearest_rounds_half_up_and_up_down_follow_their_names() {
        // 7m30s is exactly half an increment: half-up.
        let day = vec![RawEntry {
            raw_micros: 450 * MICROS,
            key: "a".into(),
        }];
        assert_eq!(1, round_day(&day, &params(Rounding::Nearest))[0].units);
        let day = vec![RawEntry {
            raw_micros: 449 * MICROS,
            key: "a".into(),
        }];
        let outcome = round_day(&day, &params(Rounding::Nearest));
        assert!(outcome[0].dropped, "rounds to nothing, so it is dropped");
        assert_eq!(1, round_day(&day, &params(Rounding::Up))[0].units);
        assert!(round_day(&day, &params(Rounding::Down))[0].dropped);
        let day = raw(&[1000]);
        assert_eq!(2, round_day(&day, &params(Rounding::Up))[0].units);
        assert_eq!(1, round_day(&day, &params(Rounding::Down))[0].units);
        assert_eq!(1, round_day(&day, &params(Rounding::Nearest))[0].units);
    }

    #[test]
    fn balanced_gives_leftover_increments_to_the_largest_remainders() {
        // Three entries of 10m: each would round to 15m (45m) but the day is
        // 30m, so only two get an increment, and ties go to the lower key.
        let day: Vec<_> = (0..3)
            .map(|index| RawEntry {
                raw_micros: 600 * MICROS,
                key: format!("e{index}"),
            })
            .collect();
        let outcome = round_day(&day, &params(Rounding::Balanced));
        let units: Vec<_> = outcome.iter().map(|result| result.units).collect();
        assert_eq!(vec![1, 1, 0], units);
        assert!(outcome[2].dropped);
        // With a larger raw value the tie breaks toward it.
        let day = vec![
            RawEntry {
                raw_micros: 600 * MICROS,
                key: "a".into(),
            },
            RawEntry {
                raw_micros: 600 * MICROS + 5,
                key: "b".into(),
            },
        ];
        let outcome = round_day(&day, &params(Rounding::Balanced));
        assert_eq!(
            vec![0, 1],
            outcome.iter().map(|r| r.units).collect::<Vec<_>>()
        );
    }

    #[test]
    fn balancing_is_noted_only_where_it_changed_the_rounding() {
        // A lone entry balances to exactly what nearest gives it.
        let outcome = round_day(&raw(&[1000]), &params(Rounding::Balanced));
        assert!(!outcome[0].balanced);
        // Three 10m entries: nearest would give 15m each; balancing gives
        // 15m, 15m and nothing, so the third differs (and so does no other).
        let day: Vec<_> = (0..3)
            .map(|index| RawEntry {
                raw_micros: 600 * MICROS,
                key: format!("e{index}"),
            })
            .collect();
        let outcome = round_day(&day, &params(Rounding::Balanced));
        assert_eq!(
            vec![false, false, true],
            outcome.iter().map(|r| r.balanced).collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_rounded_entry_is_a_whole_number_of_increments_by_construction() {
        let mut random = Lcg(7);
        for _ in 0..500 {
            let day = random_day(&mut random);
            for rounding in [
                Rounding::Nearest,
                Rounding::Up,
                Rounding::Down,
                Rounding::Balanced,
            ] {
                let outcome = round_day(&day, &params(rounding));
                assert_eq!(day.len(), outcome.len());
                for (entry, result) in day.iter().zip(&outcome) {
                    let rounded = result.units * INC * MICROS;
                    // Never further than one increment from the raw value.
                    assert!(
                        rounded.abs_diff(entry.raw_micros) <= INC * MICROS,
                        "{rounding:?} {entry:?} {result:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn balanced_keeps_each_entry_within_an_increment_and_the_day_within_half() {
        let mut random = Lcg(11);
        for _ in 0..1000 {
            let day = random_day(&mut random);
            let outcome = round_day(&day, &params(Rounding::Balanced));
            let raw_total: u64 = day.iter().map(|entry| entry.raw_micros).sum();
            let rounded_total: u64 =
                outcome.iter().map(|result| result.units).sum::<u64>() * INC * MICROS;
            assert!(
                rounded_total.abs_diff(raw_total) <= INC * MICROS / 2,
                "{day:?} {outcome:?}"
            );
            for (entry, result) in day.iter().zip(&outcome) {
                assert!(
                    (result.units * INC * MICROS).abs_diff(entry.raw_micros) < INC * MICROS,
                    "{day:?} {outcome:?}"
                );
            }
        }
    }

    #[test]
    fn the_minimum_raises_and_drop_below_removes() {
        let p = RoundParams {
            increment_seconds: INC,
            rounding: Rounding::Nearest,
            min_entry_seconds: 1800,
            drop_below_seconds: 120,
        };
        let day = raw(&[60, 300, 1000, 3600]);
        let outcome = round_day(&day, &p);
        assert!(outcome[0].dropped, "below drop_below");
        assert!(
            outcome[1].raised && outcome[1].units == 2,
            "5m rounds to 0, raised to 30m"
        );
        assert!(
            outcome[2].raised && outcome[2].units == 2,
            "1 increment is under 30m"
        );
        assert!(!outcome[3].raised && outcome[3].units == 4);
    }

    #[test]
    fn a_minimum_that_is_not_a_multiple_rounds_up_to_the_next_increment() {
        let p = RoundParams {
            increment_seconds: INC,
            rounding: Rounding::Nearest,
            min_entry_seconds: 1000,
            drop_below_seconds: 0,
        };
        assert_eq!(2, round_day(&raw(&[400]), &p)[0].units);
    }

    fn entry(engagement: &str, raw: f64, estimated: u64, billable: bool) -> TimesheetEntry {
        TimesheetEntry {
            date: NaiveDate::from_ymd_opt(2026, 8, 12).unwrap(),
            engagement: engagement.to_string(),
            detail: None,
            label: engagement.to_string(),
            client: None,
            billable,
            raw_seconds: raw,
            estimated_seconds: estimated,
            manual_seconds: 0,
            override_seconds: None,
            final_seconds: estimated,
            first_start: None,
            last_end: None,
            evidence: Evidence::default(),
            rate: None,
            currency: None,
            amount: None,
            notes: Vec::new(),
            description: None,
            status: EntryStatus::Suggested,
            adjustments: Vec::new(),
            lock_drift_seconds: None,
        }
    }

    fn total(entries: &[TimesheetEntry]) -> u64 {
        entries.iter().map(|entry| entry.estimated_seconds).sum()
    }

    #[test]
    fn the_cap_takes_from_the_entry_rounded_up_the_most() {
        // a: 40m raw rounded to 45m (+5m). b: 50m raw rounded to 60m (+10m).
        let mut entries = vec![
            entry("a", 2400.0, 2700, true),
            entry("b", 3000.0, 3600, true),
        ];
        let warning = enforce_daily_cap(&mut entries, INC, 5400);
        assert!(warning.is_none());
        assert_eq!(5400, total(&entries));
        assert_eq!(2700, entries[0].estimated_seconds);
        assert_eq!(2700, entries[1].estimated_seconds);
        assert_eq!(vec![Adjustment::Capped], entries[1].adjustments);
        assert!(entries[0].adjustments.is_empty());
    }

    #[test]
    fn capping_ties_go_to_non_billable_first_then_smaller_raw() {
        let mut entries = vec![
            entry("billable", 3000.0, 3600, true),
            entry("internal", 3000.0, 3600, false),
        ];
        enforce_daily_cap(&mut entries, INC, 6300);
        assert_eq!(3600, entries[0].estimated_seconds);
        assert_eq!(2700, entries[1].estimated_seconds);

        let mut entries = vec![
            entry("big", 3000.0, 3600, true),
            entry("small", 2700.0, 3600, true),
        ];
        // The smaller raw value was rounded up more, so it goes first anyway.
        enforce_daily_cap(&mut entries, INC, 6300);
        assert_eq!(2700, entries[1].estimated_seconds);
    }

    #[test]
    fn manual_hours_and_overrides_are_never_reduced_and_count_against_the_cap() {
        let mut entries = vec![
            entry("a", 3600.0, 3600, true),
            entry("b", 3600.0, 3600, true),
        ];
        entries[0].manual_seconds = 1800;
        entries[1].override_seconds = Some(5400);
        let warning = enforce_daily_cap(&mut entries, INC, 6300);
        // 1800 manual + 5400 override already exceed the cap; the estimate of
        // `a` is cut to nothing, the others are untouched, and it is said.
        assert_eq!(0, entries[0].estimated_seconds);
        assert_eq!(1800, entries[0].manual_seconds);
        assert_eq!(
            3600, entries[1].estimated_seconds,
            "overridden estimates are not touched"
        );
        assert_eq!(Some(5400), entries[1].override_seconds);
        assert!(warning.unwrap().contains("exceed the daily cap"));
    }

    #[test]
    fn the_cap_is_never_exceeded_by_estimates_and_stays_in_increments() {
        let mut random = Lcg(23);
        for _ in 0..500 {
            let day = random_day(&mut random);
            let outcome = round_day(&day, &params(Rounding::Nearest));
            let mut entries: Vec<_> = day
                .iter()
                .zip(&outcome)
                .map(|(raw, result)| {
                    entry(
                        &raw.key,
                        raw.raw_micros as f64 / 1e6,
                        result.units * INC,
                        random.next(2) == 0,
                    )
                })
                .collect();
            let cap = (1 + random.next(24)) * INC;
            let manual = random.next(3) * 600;
            if let Some(first) = entries.first_mut() {
                first.manual_seconds = manual;
            }
            enforce_daily_cap(&mut entries, INC, cap);
            let estimated = total(&entries);
            assert!(
                estimated + manual <= cap.max(manual),
                "{estimated} {manual} {cap}"
            );
            assert!(
                entries
                    .iter()
                    .all(|entry| entry.estimated_seconds % INC == 0)
            );
            assert_eq!(
                manual,
                entries.first().map_or(0, |entry| entry.manual_seconds)
            );
        }
    }
}
