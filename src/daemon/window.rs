//! Daily windows and daylight saving: a line-for-line port of the shell's
//! `routine_window_state` (`libexec/sdsync-common`).
//!
//! A daily window belongs to the calendar day its START lies on, and every
//! question about it -- is it open, is it due, which day does a run record --
//! is answered by one evaluation from one instant.
//!
//! - START <= END: open while the wall time is in [START, END]; the day is today.
//! - START > END (it wraps past midnight): open from START to midnight as today's
//!   window, and from midnight to END as yesterday's. The weekday filter is applied
//!   to that day: a Saturday 23:00-01:00 window is not Sunday's, and an unselected
//!   Sunday's tail does not run on Monday.
//! - Due when open, its day's weekday is selected and LAST_DAY is not its day.
//!
//! Daylight saving (the five decisions the shell made, kept):
//! - Yesterday is the date of an instant about noon yesterday: the wall minutes
//!   since midnight plus twelve hours back.
//! - On a fall-back night the wall clock can re-enter yesterday's tail after
//!   today's window opened (01:30-01:00); a later window that has run closes it.
//! - A start that occurs twice has one day, so it dispatches once.
//! - A window with no wall minute on a spring-forward day opens at the first
//!   instant after the gap, with [`WindowNote::DstGap`] so the dispatch can say so.
//! - Dates come only from epoch -> local, never from parsing a wall-clock string.
//!
//! Local time comes from a [`LocalClock`], so tests inject real zones from TZif
//! fixtures and production uses the C library (`sys::LibcLocalClock`).

#![forbid(unsafe_code)]

/// The wall-clock fields of one instant in the local zone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WallFields {
    /// `YYYY-MM-DD`, the local calendar day.
    pub(crate) day: String,
    /// ISO weekday, 1 (Monday) ..= 7 (Sunday), as `date +%u`.
    pub(crate) weekday: u8,
    /// Minutes since local midnight, 0 ..= 1439.
    pub(crate) minute: i64,
    /// Minutes east of UTC, as `date +%z` read back.
    pub(crate) offset: i64,
}

/// Epoch -> local wall fields. `None` means the clock cannot be read.
pub(crate) trait LocalClock {
    fn fields(&self, epoch: i64) -> Option<WallFields>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowNote {
    /// The window had no wall minute today (spring forward) and opened after the gap.
    DstGap,
}

impl WindowNote {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DstGap => "dst_gap",
        }
    }
}

/// `rw_open`, `rw_due`, `rw_day`, `rw_weekday`, `rw_note`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WindowState {
    pub(crate) open: bool,
    pub(crate) due: bool,
    pub(crate) day: String,
    pub(crate) weekday: u8,
    pub(crate) note: Option<WindowNote>,
}

/// Why a window could not be evaluated. The shell's statuses are kept for logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowError {
    /// START or END is not `HH:MM` (shell status 64).
    InvalidTime,
    /// The local date and time could not be read (shell status 73). Fail closed:
    /// never a dispatch without its window day.
    ClockUnavailable,
}

impl WindowError {
    pub(crate) fn status(self) -> u8 {
        match self {
            Self::InvalidTime => 64,
            Self::ClockUnavailable => 73,
        }
    }
}

/// `rw_minutes`: `[0-2][0-9]:[0-5][0-9]` with an hour of at most 23.
pub(crate) fn parse_minutes(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || !(b'0'..=b'2').contains(&bytes[0])
        || !bytes[1].is_ascii_digit()
        || !(b'0'..=b'5').contains(&bytes[3])
        || !bytes[4].is_ascii_digit()
    {
        return None;
    }
    let hour = i64::from(bytes[0] - b'0') * 10 + i64::from(bytes[1] - b'0');
    let minute = i64::from(bytes[3] - b'0') * 10 + i64::from(bytes[4] - b'0');
    (hour <= 23).then_some(hour * 60 + minute)
}

/// `rw_minute_exists NOW T M DAY`: whether wall minute M exists on DAY, the day
/// of NOW, whose wall minute is T. The candidate instant NOW - (T - M) minutes,
/// corrected once by the offset difference, must round-trip to M on DAY.
fn minute_exists(
    clock: &dyn LocalClock,
    now: i64,
    now_offset: i64,
    wall: i64,
    minute: i64,
    day: &str,
) -> Result<bool, WindowError> {
    let mut candidate = now - (wall - minute) * 60;
    let fields = clock
        .fields(candidate)
        .ok_or(WindowError::ClockUnavailable)?;
    if fields.minute == minute && fields.day == day {
        return Ok(true);
    }
    candidate += (now_offset - fields.offset) * 60;
    let fields = clock
        .fields(candidate)
        .ok_or(WindowError::ClockUnavailable)?;
    Ok(fields.minute == minute && fields.day == day)
}

/// `routine_window_state WEEKDAYS START END NOW_EPOCH LAST_DAY`.
pub(crate) fn window_state(
    weekdays: &str,
    start: &str,
    end: &str,
    now: i64,
    last_day: &str,
    clock: &dyn LocalClock,
) -> Result<WindowState, WindowError> {
    let start = parse_minutes(start).ok_or(WindowError::InvalidTime)?;
    let end = parse_minutes(end).ok_or(WindowError::InvalidTime)?;
    if now < 0 {
        return Err(WindowError::ClockUnavailable);
    }
    let fields = clock.fields(now).ok_or(WindowError::ClockUnavailable)?;
    let mut state = WindowState {
        open: false,
        due: false,
        day: fields.day,
        weekday: fields.weekday,
        note: None,
    };
    let wall = fields.minute;
    let now_offset = fields.offset;
    if start <= end {
        if wall < start {
            return Ok(state);
        }
        if wall > end {
            // Closed, unless no wall minute of [START, END] existed today: a
            // window wholly inside a spring-forward gap. Asked only while
            // today's window is still owed and the offset grew in the last day.
            if last_day == state.day {
                return Ok(state);
            }
            let yesterday = clock
                .fields(now - 86_400)
                .ok_or(WindowError::ClockUnavailable)?;
            if yesterday.offset >= now_offset {
                return Ok(state);
            }
            if minute_exists(clock, now, now_offset, wall, end, &state.day)? {
                return Ok(state);
            }
            if minute_exists(clock, now, now_offset, wall, start, &state.day)? {
                return Ok(state);
            }
            state.note = Some(WindowNote::DstGap);
        }
    } else if wall >= start {
        // Today's window, START to midnight.
    } else if wall <= end {
        // Yesterday's tail. Today's window has already run: a later window that
        // has run closes every earlier one.
        if last_day == state.day {
            return Ok(state);
        }
        let yesterday = clock
            .fields(now - (wall + 720) * 60)
            .ok_or(WindowError::ClockUnavailable)?;
        state.day = yesterday.day;
        state.weekday = yesterday.weekday;
    } else {
        return Ok(state);
    }
    let selected = state.weekday.to_string();
    if !weekdays.split(',').any(|entry| entry == selected) {
        return Ok(state);
    }
    state.open = true;
    state.due = last_day != state.day;
    Ok(state)
}

/// `clamp_clock_step SCOPE FIELD VALUE NOW HORIZON`: a stored instant further
/// ahead of the clock than any deadline the scheduler writes can only be a clock
/// that stepped backwards. Returns the clamped value (now) when it clamped.
pub(crate) fn clamp_clock_step(value: i64, now: i64, horizon: i64) -> Option<i64> {
    (value > now.saturating_add(horizon).saturating_add(60)).then_some(now)
}

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    // Both are bounded by construction (1..=12, 1..=31).
    (year, month as u32, day as u32)
}

/// (year, month, day) -> days since 1970-01-01, proleptic Gregorian.
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i64::from(month);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The wall fields of `epoch` at a fixed UTC offset of `offset_seconds`.
pub(crate) fn fields_at_offset(epoch: i64, offset_seconds: i64) -> WallFields {
    let local = epoch + offset_seconds;
    let days = local.div_euclid(86_400);
    let seconds = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday (ISO 4).
    let weekday = (days + 3).rem_euclid(7) as u8 + 1;
    WallFields {
        day: format!("{year:04}-{month:02}-{day:02}"),
        weekday,
        minute: seconds / 60,
        // `date +%z` carries hours and minutes only; truncate toward zero as the
        // shell's arithmetic on its digits does.
        offset: offset_seconds / 60,
    }
}

/// A zone with one fixed offset, such as UTC.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FixedOffsetClock {
    pub(crate) offset_seconds: i64,
}

impl LocalClock for FixedOffsetClock {
    fn fields(&self, epoch: i64) -> Option<WallFields> {
        Some(fields_at_offset(epoch, self.offset_seconds))
    }
}

#[cfg(test)]
#[path = "tzif.rs"]
pub(crate) mod tzif;

#[cfg(test)]
mod tests {
    use super::tzif::TzifClock;
    use super::*;

    fn zone(name: &str) -> TzifClock {
        let bytes: &[u8] = match name {
            "America/New_York" => include_bytes!("testdata/tzif/America_New_York"),
            "Europe/Lisbon" => include_bytes!("testdata/tzif/Europe_Lisbon"),
            "Europe/Berlin" => include_bytes!("testdata/tzif/Europe_Berlin"),
            "Australia/Lord_Howe" => include_bytes!("testdata/tzif/Australia_Lord_Howe"),
            other => panic!("no fixture for {other}"),
        };
        TzifClock::parse(bytes).unwrap()
    }

    const UTC: FixedOffsetClock = FixedOffsetClock { offset_seconds: 0 };

    /// `YYYY-MM-DDTHH:MM` local -> epoch, for an unambiguous wall time.
    fn local_epoch(clock: &dyn LocalClock, text: &str) -> i64 {
        let year = text[0..4].parse().unwrap();
        let month = text[5..7].parse().unwrap();
        let day = text[8..10].parse().unwrap();
        let hour: i64 = text[11..13].parse().unwrap();
        let minute: i64 = text[14..16].parse().unwrap();
        let naive = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60;
        // Two corrections converge for any wall time that exists exactly once.
        let mut guess = naive;
        for _ in 0..3 {
            let offset = clock.fields(guess).unwrap().offset * 60;
            guess = naive - offset;
        }
        let back = clock.fields(guess).unwrap();
        assert_eq!(back.minute, hour * 60 + minute, "{text} is not a wall time");
        guess
    }

    /// `Sun 2026-03-08 03:00` -- the shape the shell suite compares against.
    fn shown(clock: &dyn LocalClock, epoch: i64) -> String {
        let fields = clock.fields(epoch).unwrap();
        let name =
            ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][usize::from(fields.weekday) - 1];
        format!(
            "{name} {} {:02}:{:02}",
            fields.day,
            fields.minute / 60,
            fields.minute % 60
        )
    }

    /// The shell suite's `F3_WINDOW_DRIVER`: step a fake instant from FIRST to
    /// LAST, starting with `last=none`, and record every due evaluation.
    fn window_runs(
        clock: &dyn LocalClock,
        weekdays: &str,
        start: &str,
        end: &str,
        first: &str,
        last: &str,
        step: i64,
    ) -> Vec<(i64, String, &'static str)> {
        let mut now = local_epoch(clock, first);
        let finish = local_epoch(clock, last);
        let mut last_day = "none".to_owned();
        let mut runs = Vec::new();
        while now <= finish {
            let state = window_state(weekdays, start, end, now, &last_day, clock).unwrap();
            if state.due {
                let note = state.note.map_or("none", WindowNote::as_str);
                runs.push((now, state.day.clone(), note));
                last_day = state.day;
            }
            now += step;
        }
        runs
    }

    #[test]
    fn tzif_fixtures_cover_the_tested_years_and_know_the_transitions() {
        for name in [
            "America/New_York",
            "Europe/Lisbon",
            "Europe/Berlin",
            "Australia/Lord_Howe",
        ] {
            let clock = zone(name);
            // Fat TZif files list transitions to 2037; the tests need 2026.
            assert!(clock.last_transition() > 1_830_297_600, "{name}"); // 2028-01-01
        }
        // 2026-03-08 06:59:59 UTC is 01:59:59 EST; one second later is 03:00 EDT.
        let ny = zone("America/New_York");
        assert_eq!(ny.fields(1_772_953_199).unwrap().offset, -300);
        assert_eq!(ny.fields(1_772_953_200).unwrap().offset, -240);
        assert_eq!(shown(&ny, 1_772_953_200), "Sun 2026-03-08 03:00");
        // Lord Howe moves by thirty minutes: +10:30 standard, +11 summer.
        let lh = zone("Australia/Lord_Howe");
        assert_eq!(
            lh.fields(local_epoch(&lh, "2026-07-01T12:00"))
                .unwrap()
                .offset,
            630
        );
        assert_eq!(
            lh.fields(local_epoch(&lh, "2026-12-01T12:00"))
                .unwrap()
                .offset,
            660
        );
        // Lisbon: WET in winter, WEST in summer.
        let lisbon = zone("Europe/Lisbon");
        assert_eq!(
            lisbon
                .fields(local_epoch(&lisbon, "2026-01-15T12:00"))
                .unwrap()
                .offset,
            0
        );
        assert_eq!(
            lisbon
                .fields(local_epoch(&lisbon, "2026-07-15T12:00"))
                .unwrap()
                .offset,
            60
        );
    }

    #[test]
    fn civil_dates_round_trip_and_weekdays_are_iso() {
        for days in [-1_000_000, -1, 0, 1, 20_454, 2_932_896] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
        // 2026-11-03 is a Tuesday.
        let fields = fields_at_offset(days_from_civil(2026, 11, 3) * 86_400, 0);
        assert_eq!(fields.day, "2026-11-03");
        assert_eq!(fields.weekday, 2);
        assert_eq!(fields_at_offset(0, 0).weekday, 4);
    }

    #[test]
    fn times_parse_with_the_shells_shape_and_bounds() {
        assert_eq!(parse_minutes("00:00"), Some(0));
        assert_eq!(parse_minutes("23:59"), Some(1_439));
        assert_eq!(parse_minutes("09:05"), Some(545));
        for bad in [
            "24:00", "29:00", "1:00", "01:60", "01-00", "", "01:000", "3a:00",
        ] {
            assert_eq!(parse_minutes(bad), None, "{bad}");
        }
        assert_eq!(
            window_state("1", "25:00", "01:00", 0, "none", &UTC),
            Err(WindowError::InvalidTime)
        );
        assert_eq!(WindowError::InvalidTime.status(), 64);
    }

    struct BrokenClock;
    impl LocalClock for BrokenClock {
        fn fields(&self, _epoch: i64) -> Option<WallFields> {
            None
        }
    }

    /// I-4: an unreadable clock never dispatches.
    #[test]
    fn unreadable_clock_fails_closed() {
        assert_eq!(
            window_state(
                "1,2,3,4,5,6,7",
                "00:00",
                "23:59",
                1_000,
                "none",
                &BrokenClock
            ),
            Err(WindowError::ClockUnavailable)
        );
        assert_eq!(
            window_state("1", "00:00", "23:59", -5, "none", &UTC),
            Err(WindowError::ClockUnavailable)
        );
        assert_eq!(WindowError::ClockUnavailable.status(), 73);
    }

    /// G-6: a wrapping window runs on its own day's weekday only.
    #[test]
    fn a_wrapping_window_runs_on_its_own_days_weekday_only() {
        let sunday = window_runs(
            &UTC,
            "7",
            "23:00",
            "01:00",
            "2026-09-28T02:00",
            "2026-10-19T02:00",
            900,
        );
        assert_eq!(
            sunday
                .iter()
                .map(|(e, _, _)| shown(&UTC, *e))
                .collect::<Vec<_>>(),
            [
                "Sun 2026-10-04 23:00",
                "Sun 2026-10-11 23:00",
                "Sun 2026-10-18 23:00"
            ]
        );
        assert_eq!(
            sunday
                .iter()
                .map(|(_, d, _)| d.as_str())
                .collect::<Vec<_>>(),
            ["2026-10-04", "2026-10-11", "2026-10-18"]
        );
        let weekdays = window_runs(
            &UTC,
            "1,2,3,4,5",
            "23:00",
            "01:00",
            "2026-09-28T02:00",
            "2026-10-12T02:00",
            900,
        );
        assert_eq!(weekdays.len(), 10);
        for (epoch, _, _) in &weekdays {
            let local = shown(&UTC, *epoch);
            assert!(local.ends_with(" 23:00"), "{local}");
            assert!(
                !local.starts_with("Sat") && !local.starts_with("Sun"),
                "{local}"
            );
        }
    }

    /// The unselected weekday's tail does not run on the next (selected) day.
    #[test]
    fn an_unselected_weekdays_tail_does_not_run_the_next_morning() {
        // 2026-10-04 is a Sunday. A Monday-only 23:00-01:00 window: Sunday 23:00
        // opens Sunday's (unselected) window, Monday 00:30 is still Sunday's tail.
        let monday_half_past = local_epoch(&UTC, "2026-10-05T00:30");
        let state = window_state("1", "23:00", "01:00", monday_half_past, "none", &UTC).unwrap();
        assert_eq!(state.day, "2026-10-04");
        assert_eq!(state.weekday, 7);
        assert!(!state.open && !state.due);
        // Monday 23:00 is Monday's own window.
        let monday_night = local_epoch(&UTC, "2026-10-05T23:00");
        let state = window_state("1", "23:00", "01:00", monday_night, "none", &UTC).unwrap();
        assert!(state.open && state.due);
        assert_eq!(state.day, "2026-10-05");
    }

    /// G-7: daylight saving never repeats or skips a window day, for overnight and
    /// same-day windows across both transitions of each zone.
    #[test]
    fn daylight_saving_never_repeats_or_skips_a_window_day() {
        let cases: &[(&str, [(&str, &str); 2])] = &[
            (
                "America/New_York",
                [
                    ("2026-10-30T12:00", "2026-11-03T12:00"),
                    ("2026-03-06T12:00", "2026-03-10T12:00"),
                ],
            ),
            (
                "Europe/Berlin",
                [
                    ("2026-10-23T12:00", "2026-10-27T12:00"),
                    ("2026-03-27T12:00", "2026-03-31T12:00"),
                ],
            ),
            (
                "Europe/Lisbon",
                [
                    ("2026-10-23T12:00", "2026-10-27T12:00"),
                    ("2026-03-27T12:00", "2026-03-31T12:00"),
                ],
            ),
            (
                "Australia/Lord_Howe",
                [
                    ("2026-04-02T12:00", "2026-04-06T12:00"),
                    ("2026-10-02T12:00", "2026-10-06T12:00"),
                ],
            ),
        ];
        for (name, spans) in cases {
            let clock = zone(name);
            for (first, last) in spans {
                for (start, end) in [("23:00", "02:30"), ("01:00", "03:00")] {
                    let runs = window_runs(&clock, "1,2,3,4,5,6,7", start, end, first, last, 900);
                    let label = format!("{name} {start}-{end} from {first}");
                    let local = runs
                        .iter()
                        .map(|(e, _, _)| shown(&clock, *e))
                        .collect::<Vec<_>>();
                    assert_eq!(runs.len(), 4, "{label}: {local:?}");
                    let days = runs.iter().map(|(_, d, _)| d.clone()).collect::<Vec<_>>();
                    let mut unique = days.clone();
                    unique.dedup();
                    assert_eq!(unique.len(), days.len(), "{label}: {local:?}");
                    for pair in days.windows(2) {
                        let parse = |d: &str| {
                            days_from_civil(
                                d[0..4].parse().unwrap(),
                                d[5..7].parse().unwrap(),
                                d[8..10].parse().unwrap(),
                            )
                        };
                        assert_eq!(parse(&pair[1]) - parse(&pair[0]), 1, "{label}: {local:?}");
                    }
                    for ((_, day, note), shown_local) in runs.iter().zip(&local) {
                        assert_eq!(*note, "none", "{label}: {local:?}");
                        let wall_day = &shown_local[4..14];
                        if start > end {
                            let next = civil_from_days(
                                days_from_civil(
                                    day[0..4].parse().unwrap(),
                                    day[5..7].parse().unwrap(),
                                    day[8..10].parse().unwrap(),
                                ) + 1,
                            );
                            let next = format!("{:04}-{:02}-{:02}", next.0, next.1, next.2);
                            assert!(wall_day == day || wall_day == next, "{label}: {local:?}");
                        } else {
                            assert_eq!(wall_day, day, "{label}: {local:?}");
                        }
                    }
                }
            }
        }
    }

    /// A near-whole-day window runs once on a fall-back night, and a window wholly
    /// inside the repeated hour runs once, at its first occurrence.
    #[test]
    fn a_near_whole_day_window_runs_once_on_a_fall_back_night() {
        let ny = zone("America/New_York");
        let wide = window_runs(
            &ny,
            "1,2,3,4,5,6,7",
            "01:30",
            "01:00",
            "2026-10-31T12:00",
            "2026-11-02T12:00",
            300,
        );
        assert_eq!(
            wide.iter()
                .map(|(e, _, _)| shown(&ny, *e))
                .collect::<Vec<_>>(),
            [
                "Sat 2026-10-31 12:00",
                "Sun 2026-11-01 01:30",
                "Mon 2026-11-02 01:30"
            ]
        );
        // The first 01:30 that night is EDT (the repeated hour comes after it).
        assert_eq!(ny.fields(wide[1].0).unwrap().offset, -240);
        assert_eq!(ny.fields(wide[2].0).unwrap().offset, -300);
        let inside = window_runs(
            &ny,
            "1,2,3,4,5,6,7",
            "01:00",
            "01:30",
            "2026-10-31T12:00",
            "2026-11-02T00:00",
            300,
        );
        assert_eq!(inside.len(), 1);
        assert_eq!(shown(&ny, inside[0].0), "Sun 2026-11-01 01:00");
        assert_eq!(ny.fields(inside[0].0).unwrap().offset, -240);

        // Lisbon's repeated hour is 01:00-02:00 on 2026-10-25.
        let lisbon = zone("Europe/Lisbon");
        let inside = window_runs(
            &lisbon,
            "1,2,3,4,5,6,7",
            "01:10",
            "01:40",
            "2026-10-24T12:00",
            "2026-10-26T00:00",
            300,
        );
        assert_eq!(inside.len(), 1);
        assert_eq!(shown(&lisbon, inside[0].0), "Sun 2026-10-25 01:10");
        assert_eq!(lisbon.fields(inside[0].0).unwrap().offset, 60);

        // Lord Howe repeats 01:30-02:00 on 2026-04-05 (thirty-minute shift).
        let lh = zone("Australia/Lord_Howe");
        let inside = window_runs(
            &lh,
            "1,2,3,4,5,6,7",
            "01:35",
            "01:55",
            "2026-04-04T12:00",
            "2026-04-06T00:00",
            300,
        );
        assert_eq!(inside.len(), 1);
        assert_eq!(shown(&lh, inside[0].0), "Sun 2026-04-05 01:35");
        assert_eq!(lh.fields(inside[0].0).unwrap().offset, 660);
    }

    /// A window inside a spring-forward gap runs once, right after the gap, and
    /// says so (`dst_gap`); a window with a surviving minute opens at that minute.
    #[test]
    fn a_window_inside_a_spring_forward_gap_runs_once_after_it_and_says_so() {
        let ny = zone("America/New_York");
        let runs = window_runs(
            &ny,
            "1,2,3,4,5,6,7",
            "02:00",
            "02:30",
            "2026-03-07T12:00",
            "2026-03-09T12:00",
            300,
        );
        assert_eq!(
            runs.iter()
                .map(|(e, d, n)| (shown(&ny, *e), d.as_str(), *n))
                .collect::<Vec<_>>(),
            [
                ("Sun 2026-03-08 03:00".to_owned(), "2026-03-08", "dst_gap"),
                ("Mon 2026-03-09 02:00".to_owned(), "2026-03-09", "none"),
            ]
        );

        let berlin = zone("Europe/Berlin");
        let gap = window_runs(
            &berlin,
            "1,2,3,4,5,6,7",
            "02:00",
            "02:30",
            "2026-03-28T12:00",
            "2026-03-30T12:00",
            300,
        );
        assert_eq!(
            gap.iter()
                .map(|(e, _, n)| (shown(&berlin, *e), *n))
                .collect::<Vec<_>>(),
            [
                ("Sun 2026-03-29 03:00".to_owned(), "dst_gap"),
                ("Mon 2026-03-30 02:00".to_owned(), "none"),
            ]
        );
        let part = window_runs(
            &berlin,
            "1,2,3,4,5,6,7",
            "02:15",
            "03:15",
            "2026-03-28T12:00",
            "2026-03-30T12:00",
            300,
        );
        assert_eq!(
            part.iter()
                .map(|(e, _, n)| (shown(&berlin, *e), *n))
                .collect::<Vec<_>>(),
            [
                ("Sun 2026-03-29 03:00".to_owned(), "none"),
                ("Mon 2026-03-30 02:15".to_owned(), "none"),
            ]
        );

        // Lisbon springs forward 01:00 -> 02:00 on 2026-03-29.
        let lisbon = zone("Europe/Lisbon");
        let gap = window_runs(
            &lisbon,
            "1,2,3,4,5,6,7",
            "01:15",
            "01:45",
            "2026-03-28T12:00",
            "2026-03-30T12:00",
            300,
        );
        assert_eq!(
            gap.iter()
                .map(|(e, d, n)| (shown(&lisbon, *e), d.as_str(), *n))
                .collect::<Vec<_>>(),
            [
                ("Sun 2026-03-29 02:00".to_owned(), "2026-03-29", "dst_gap"),
                ("Mon 2026-03-30 01:15".to_owned(), "2026-03-30", "none"),
            ]
        );

        // Lord Howe springs forward 02:00 -> 02:30 on 2026-10-04.
        let lh = zone("Australia/Lord_Howe");
        let gap = window_runs(
            &lh,
            "1,2,3,4,5,6,7",
            "02:00",
            "02:20",
            "2026-10-03T12:00",
            "2026-10-05T12:00",
            300,
        );
        assert_eq!(
            gap.iter()
                .map(|(e, _, n)| (shown(&lh, *e), *n))
                .collect::<Vec<_>>(),
            [
                ("Sun 2026-10-04 02:30".to_owned(), "dst_gap"),
                ("Mon 2026-10-05 02:00".to_owned(), "none"),
            ]
        );
    }

    /// A gap window already run today is not run again after the gap.
    #[test]
    fn a_gap_window_that_already_ran_today_stays_closed() {
        let ny = zone("America/New_York");
        let after_gap = local_epoch(&ny, "2026-03-08T03:10");
        let state = window_state(
            "1,2,3,4,5,6,7",
            "02:00",
            "02:30",
            after_gap,
            "2026-03-08",
            &ny,
        )
        .unwrap();
        assert!(!state.open && !state.due);
        assert_eq!(state.note, None);
    }

    #[test]
    fn clock_step_clamp_matches_the_shell() {
        assert_eq!(clamp_clock_step(1_000, 1_000, 3_600), None);
        assert_eq!(clamp_clock_step(1_000 + 3_660, 1_000, 3_600), None);
        assert_eq!(clamp_clock_step(1_000 + 3_661, 1_000, 3_600), Some(1_000));
        assert_eq!(clamp_clock_step(1_061, 1_000, 0), Some(1_000));
    }
}
