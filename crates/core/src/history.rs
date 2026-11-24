//! Battery history: which readings become samples ([`Recorder`]), their `history.tsv`
//! line ([`Sample::to_line`], [`Sample::parse`]), the samples kept in memory ([`add`]) and
//! the time-left estimate ([`time_left`]).
//!
//! Samples come from the snapshot only (readings that happened anyway), never from device
//! traffic of their own. Their times are wall-clock unix milliseconds, unlike the
//! monotonic [`Ms`](crate::time::Ms) of the readings.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use crate::model::{Charge, CoarseLevel, DeviceKey, DeviceState, Level, Presence, Source};

/// An unchanged device is recorded again at most this often, on a fresh reading.
pub const RECORD_EVERY_MS: u64 = 30 * 60 * 1000;
/// Samples are kept this long (30 days).
pub const KEEP_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// A time-left estimate needs a discharge span at least this long (2 h)…
const MIN_SPAN_MS: u64 = 2 * 60 * 60 * 1000;
/// …with at least this many samples.
const MIN_POINTS: usize = 3;
/// Times from 2286 on are rejected: a torn line's leftover digits run on into the next
/// line's 13-digit time and make it at least 14 digits long.
const MAX_UNIX_MS: u64 = 9_999_999_999_999;

const COARSE: [(CoarseLevel, &str); 4] = [
    (CoarseLevel::Critical, "critical"),
    (CoarseLevel::Low, "low"),
    (CoarseLevel::Good, "good"),
    (CoarseLevel::Full, "full"),
];
const CHARGES: [(Charge, &str); 5] = [
    (Charge::Discharging, "discharging"),
    (Charge::Charging, "charging"),
    (Charge::Full, "full"),
    (Charge::NotCharging, "not-charging"),
    (Charge::Unknown, "unknown"),
];
const PRESENCES: [(Presence, &str); 3] = [
    (Presence::Online, "online"),
    (Presence::Asleep, "asleep"),
    (Presence::Offline, "offline"),
];
const SOURCES: [(Source, &str); 3] = [
    (Source::Push, "push"),
    (Source::Query, "query"),
    (Source::Os, "os"),
];
/// Written for a level or millivolts that are not known.
const NONE: &str = "-";

/// One reading of a device, at a wall-clock time (unix milliseconds).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    pub at_unix_ms: u64,
    pub key: DeviceKey,
    pub level: Option<Level>,
    pub charge: Charge,
    pub presence: Presence,
    pub millivolts: Option<u16>,
    pub source: Source,
}

impl Sample {
    /// The reading of `state`, taken at `at_unix_ms`.
    pub fn of(state: &DeviceState, at_unix_ms: u64) -> Sample {
        Sample {
            at_unix_ms,
            key: state.key.clone(),
            level: state.level,
            charge: state.charge,
            presence: state.presence,
            millivolts: state.millivolts,
            source: state.source,
        }
    }

    /// Its `history.tsv` line, without the line break: time, key, level (percent, a
    /// coarse level's name or `-`), charge, presence, millivolts (or `-`) and source,
    /// tab-separated. The key must hold no tab or line break ([`Recorder`] skips such keys).
    pub fn to_line(&self) -> String {
        let level = match self.level {
            None => NONE.to_owned(),
            Some(Level::Percent(p)) => p.to_string(),
            Some(Level::Coarse(c)) => name(&COARSE, c).to_owned(),
        };
        let millivolts = self
            .millivolts
            .map_or_else(|| NONE.to_owned(), |mv| mv.to_string());
        format!(
            "{}\t{}\t{level}\t{}\t{}\t{millivolts}\t{}",
            self.at_unix_ms,
            self.key.0,
            name(&CHARGES, self.charge),
            name(&PRESENCES, self.presence),
            name(&SOURCES, self.source),
        )
    }

    /// The sample of a [`Sample::to_line`] line; `None` unless it has exactly its fields,
    /// each with a value `to_line` writes (so a torn line, or one that ran on into the
    /// next, is rejected).
    pub fn parse(line: &str) -> Option<Sample> {
        let mut fields = line.split('\t');
        let mut next = || fields.next();
        let at_unix_ms = number(next()?).filter(|at| *at <= MAX_UNIX_MS)?;
        let key = next().filter(|key| !key.is_empty())?;
        let level = match next()? {
            NONE => None,
            text => Some(match number(text) {
                Some(p) => Level::Percent(p),
                None => Level::Coarse(value(&COARSE, text)?),
            }),
        };
        let charge = value(&CHARGES, next()?)?;
        let presence = value(&PRESENCES, next()?)?;
        let millivolts = match next()? {
            NONE => None,
            text => Some(number(text)?),
        };
        let source = value(&SOURCES, next()?)?;
        if next().is_some() {
            return None;
        }
        Some(Sample {
            at_unix_ms,
            key: DeviceKey(key.to_owned()),
            level,
            charge,
            presence,
            millivolts,
            source,
        })
    }

    /// Whether `other` reads the same: level, charge and presence (millivolts and the
    /// source alone are no change).
    fn same_reading(&self, other: &Sample) -> bool {
        self.level == other.level && self.charge == other.charge && self.presence == other.presence
    }

    /// A session break for `key` at `at_unix_ms`: nothing was observed from the device's
    /// previous sample until now (BatterySight was not running, or the PC was off or
    /// asleep). Written as an offline sample with nothing known (no level or millivolts,
    /// charge unknown, source OS), it ends discharge spans and chart lines like any offline
    /// sample. A real reading of an offline device that knows nothing reads the same, and
    /// means the same.
    pub fn session_break(key: DeviceKey, at_unix_ms: u64) -> Sample {
        Sample {
            at_unix_ms,
            key,
            level: None,
            charge: Charge::Unknown,
            presence: Presence::Offline,
            millivolts: None,
            source: Source::Os,
        }
    }

    /// Whether this is a session break ([`Sample::session_break`]).
    pub fn is_break(&self) -> bool {
        self.level.is_none()
            && self.charge == Charge::Unknown
            && self.presence == Presence::Offline
            && self.millivolts.is_none()
            && self.source == Source::Os
    }
}

fn name<T: PartialEq + Copy>(table: &[(T, &'static str)], of: T) -> &'static str {
    table
        .iter()
        .find(|(v, _)| *v == of)
        .map_or(NONE, |(_, n)| n)
}

fn value<T: Copy>(table: &[(T, &str)], text: &str) -> Option<T> {
    table.iter().find(|(_, n)| *n == text).map(|(v, _)| *v)
}

/// Decimal digits only (no sign, no spaces), within `T`'s range.
fn number<T: FromStr>(text: &str) -> Option<T> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Which snapshot readings become samples: a device's first reading, any change of level,
/// charge or presence, and a fresh unchanged reading [`RECORD_EVERY_MS`] or more after
/// the device's last sample (either way, so a clock set back does not stop it). After a
/// session break ([`Recorder::session_break`]) a device's next fresh reading is recorded,
/// changed or not, and a reading from before the break is not.
#[derive(Debug, Default)]
pub struct Recorder {
    /// Per device: its last sample, and the `updated_ms` of the last reading seen (a
    /// reading is fresh when it differs).
    last: HashMap<DeviceKey, (Sample, u64)>,
}

impl Recorder {
    /// The samples `states` (a snapshot, hidden devices included) give at `now_unix_ms`.
    pub fn observe(&mut self, states: &[DeviceState], now_unix_ms: u64) -> Vec<Sample> {
        let mut samples = Vec::new();
        for state in states {
            // Such a key would break its line; no engine makes one.
            if state.key.0.is_empty() || state.key.0.contains(['\t', '\n', '\r']) {
                continue;
            }
            let sample = Sample::of(state, now_unix_ms);
            let record = match self.last.get_mut(&state.key) {
                None => true,
                Some((last, seen)) => {
                    let fresh = std::mem::replace(seen, state.updated_ms) != state.updated_ms;
                    if last.is_break() {
                        fresh
                    } else {
                        !last.same_reading(&sample)
                            || (fresh && now_unix_ms.abs_diff(last.at_unix_ms) >= RECORD_EVERY_MS)
                    }
                }
            };
            if record {
                self.last
                    .insert(state.key.clone(), (sample.clone(), state.updated_ms));
                samples.push(sample);
            }
        }
        samples
    }

    /// Session breaks at `now_unix_ms` (BatterySight started, the PC is going to sleep or
    /// resumed) for every device read since its last break: those whose latest sample in
    /// `history`, or last sample in this recorder, is a reading. A second break with no
    /// reading in between adds nothing.
    pub fn session_break(
        &mut self,
        history: &HashMap<DeviceKey, Vec<Sample>>,
        now_unix_ms: u64,
    ) -> Vec<Sample> {
        let read = |sample: Option<&Sample>| sample.is_some_and(|s| !s.is_break());
        let mut keys: Vec<&DeviceKey> = history
            .iter()
            .filter(|(_, samples)| read(samples.last()))
            .map(|(key, _)| key)
            .chain(
                self.last
                    .iter()
                    .filter(|(_, (last, _))| read(Some(last)))
                    .map(|(key, _)| key),
            )
            .collect();
        keys.sort();
        keys.dedup();
        let breaks = keys
            .into_iter()
            .map(|key| Sample::session_break(key.clone(), now_unix_ms))
            .collect();
        for (last, _) in self.last.values_mut() {
            *last = Sample::session_break(last.key.clone(), now_unix_ms);
        }
        breaks
    }
}

/// The samples of `text` (a `history.tsv`) taken at or after `since_unix_ms`, in file
/// order, and whether the file holds anything else: older samples, malformed or torn
/// lines, or a last line without its line break. Rewriting it from the samples drops
/// those.
pub fn read(text: &str, since_unix_ms: u64) -> (Vec<Sample>, bool) {
    let mut dirty = !text.is_empty() && !text.ends_with('\n');
    let mut kept = Vec::new();
    for line in text.lines() {
        match Sample::parse(line) {
            Some(s) if s.at_unix_ms >= since_unix_ms => kept.push(s),
            _ => dirty = true,
        }
    }
    (kept, dirty)
}

/// Adds `samples` to `history` (each device's samples in time order) and drops every
/// device's samples older than [`KEEP_MS`] before `now_unix_ms`. A device left with session
/// breaks only is dropped: breaks alone keep nothing worth showing.
pub fn add(
    history: &mut HashMap<DeviceKey, Vec<Sample>>,
    samples: impl IntoIterator<Item = Sample>,
    now_unix_ms: u64,
) {
    for sample in samples {
        let list = history.entry(sample.key.clone()).or_default();
        // Almost always at the end; earlier after a clock set back.
        let at = list.partition_point(|s| s.at_unix_ms <= sample.at_unix_ms);
        list.insert(at, sample);
    }
    let since = now_unix_ms.saturating_sub(KEEP_MS);
    history.retain(|_, list| {
        let old = list.partition_point(|s| s.at_unix_ms < since);
        list.drain(..old);
        list.iter().any(|s| !s.is_break())
    });
}

/// How long the battery of the device with `samples` (in time order) lasts from its
/// latest level, at the rate it fell over its most recent qualifying discharge span: a
/// run of awake, discharging samples with a percentage that does not rise, lasting at
/// least 2 h with at least 3 samples. A sleeping or offline sample, a session break, a
/// charge, or a rise ends a span. The latest level is the latest sample's, session breaks
/// after it left out (a device that has not reported since the PC woke keeps its
/// estimate). `None` without such a span, without a drop over it, or when the latest
/// sample is on power or has no percentage. Samples after `now_unix_ms` are left out.
pub fn time_left(samples: &[Sample], now_unix_ms: u64) -> Option<Duration> {
    let samples = &samples[..samples.partition_point(|s| s.at_unix_ms <= now_unix_ms)];
    let samples = &samples[..samples.iter().rposition(|s| !s.is_break())? + 1];
    let latest = samples.last()?;
    let Some(Level::Percent(level)) = latest.level else {
        return None;
    };
    if matches!(
        latest.charge,
        Charge::Charging | Charge::Full | Charge::NotCharging
    ) {
        return None;
    }
    let percent = |s: &Sample| match s.level {
        Some(Level::Percent(p))
            if s.presence == Presence::Online && s.charge == Charge::Discharging =>
        {
            Some(p)
        }
        _ => None,
    };
    // Walk back from the latest sample, span by span.
    let mut end = samples.len();
    while end > 0 {
        let Some(last) = percent(&samples[end - 1]) else {
            end -= 1;
            continue;
        };
        let mut start = end - 1;
        let mut first = last;
        while start > 0 {
            match percent(&samples[start - 1]) {
                Some(p) if p >= first => {
                    first = p;
                    start -= 1;
                }
                _ => break,
            }
        }
        let span_ms = samples[end - 1].at_unix_ms - samples[start].at_unix_ms;
        if end - start >= MIN_POINTS && span_ms >= MIN_SPAN_MS {
            let drop = u64::from(first - last);
            return (drop > 0).then(|| Duration::from_millis(u64::from(level) * span_ms / drop));
        }
        end = start;
    }
    None
}

/// "Time left: about N h" (to the nearest hour), "Time left: less than 1 h" or "Time
/// left: not enough data".
pub fn time_left_text(left: Option<Duration>) -> String {
    match left.map(|d| (d.as_secs() + 1800) / 3600) {
        None => "Time left: not enough data".to_owned(),
        Some(0) => "Time left: less than 1 h".to_owned(),
        Some(hours) => format!("Time left: about {hours} h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CoarseLevel, DeviceKind, Verification, Via};

    const MIN: u64 = 60 * 1000;
    const HOUR: u64 = 60 * MIN;
    /// 2026-11-24 00:00 UTC.
    const T0: u64 = 1_795_478_400_000;

    fn sample(at: u64, percent: u8) -> Sample {
        Sample {
            at_unix_ms: at,
            key: DeviceKey("logi:4082:1".into()),
            level: Some(Level::Percent(percent)),
            charge: Charge::Discharging,
            presence: Presence::Online,
            millivolts: None,
            source: Source::Query,
        }
    }

    fn state(percent: u8, updated_ms: u64) -> DeviceState {
        DeviceState {
            key: DeviceKey("logi:4082:1".into()),
            name: "G305".into(),
            kind: DeviceKind::Mouse,
            via: Via::Receiver,
            level: Some(Level::Percent(percent)),
            millivolts: Some(3900),
            charge: Charge::Discharging,
            presence: Presence::Online,
            updated_ms,
            source: Source::Query,
            verification: Verification::HardwareTested,
        }
    }

    #[test]
    fn line_round_trip() {
        let full = Sample {
            millivolts: Some(3_912),
            source: Source::Push,
            ..sample(T0, 87)
        };
        assert_eq!(
            full.to_line(),
            format!("{T0}\tlogi:4082:1\t87\tdischarging\tonline\t3912\tpush")
        );
        let others = [
            full.clone(),
            Sample {
                level: None,
                charge: Charge::Unknown,
                presence: Presence::Offline,
                source: Source::Os,
                ..sample(T0 + 1, 0)
            },
            Sample {
                level: Some(Level::Coarse(CoarseLevel::Critical)),
                charge: Charge::NotCharging,
                presence: Presence::Asleep,
                ..sample(T0 + 2, 0)
            },
            Sample {
                level: Some(Level::Coarse(CoarseLevel::Full)),
                charge: Charge::Full,
                ..sample(T0 + 3, 0)
            },
            Sample {
                charge: Charge::Charging,
                ..sample(T0 + 4, 100)
            },
        ];
        for s in others {
            let line = s.to_line();
            assert!(!line.contains('\n'), "{line}");
            assert_eq!(Sample::parse(&line), Some(s), "{line}");
        }
    }

    #[test]
    fn malformed_lines_are_rejected() {
        let good = sample(T0, 80).to_line();
        assert!(Sample::parse(&good).is_some());
        for bad in [
            String::new(),
            format!("{good}\textra"),
            good.replace("\tquery", ""),
            good.replace("\t80\t", "\t101x\t"),
            good.replace("\t80\t", "\t+80\t"),
            good.replace("discharging", "draining"),
            good.replace("online", "Online"),
            good.replace(&T0.to_string(), "-1"),
            good.replace("logi:4082:1", ""),
        ] {
            assert_eq!(Sample::parse(&bad), None, "{bad}");
        }
    }

    #[test]
    fn torn_last_line_is_skipped() {
        let lines: Vec<String> = (0..3)
            .map(|i| sample(T0 + i * HOUR, 90).to_line())
            .collect();
        // The last append stopped half-way: no line break, missing fields.
        let torn = &lines[2][..lines[2].len() / 2];
        let text = format!("{}\n{}\n{torn}", lines[0], lines[1]);
        let (kept, dirty) = read(&text, 0);
        assert_eq!(kept, [sample(T0, 90), sample(T0 + HOUR, 90)]);
        assert!(dirty, "the torn line is to be dropped from the file");

        // The next append ran on after it: the merged line is rejected whole.
        for cut in [3, 15, lines[2].len() - 1] {
            let merged = format!("{}{}", &lines[2][..cut], lines[1]);
            assert_eq!(Sample::parse(&merged), None, "{merged}");
        }

        // A clean file within the window has nothing to drop.
        let clean = format!("{}\n{}\n", lines[0], lines[1]);
        assert_eq!(
            read(&clean, 0),
            (vec![sample(T0, 90), sample(T0 + HOUR, 90)], false)
        );
        // Older samples are dropped.
        assert_eq!(read(&clean, T0 + 1), (vec![sample(T0 + HOUR, 90)], true));
    }

    #[test]
    fn samples_on_change_and_every_30_min() {
        let mut r = Recorder::default();
        // The first reading of a device is recorded.
        assert_eq!(
            r.observe(&[state(80, 1)], T0),
            [Sample {
                millivolts: Some(3900),
                ..sample(T0, 80)
            }]
        );
        // Unchanged: not again within 30 min, even with a fresh reading.
        assert!(r.observe(&[state(80, 2)], T0 + 29 * MIN).is_empty());
        // Unchanged and no fresh reading: never repeated.
        assert!(r.observe(&[state(80, 2)], T0 + 40 * MIN).is_empty());
        // A fresh unchanged reading 30 min after the last sample is.
        assert_eq!(r.observe(&[state(80, 3)], T0 + 40 * MIN).len(), 1);
        // A change is recorded at once.
        let s = r.observe(&[state(79, 4)], T0 + 41 * MIN);
        assert_eq!(
            s.iter().map(|s| s.level).collect::<Vec<_>>(),
            [Some(Level::Percent(79))]
        );
        let asleep = DeviceState {
            presence: Presence::Asleep,
            ..state(79, 5)
        };
        assert_eq!(
            r.observe(std::slice::from_ref(&asleep), T0 + 42 * MIN)
                .len(),
            1
        );
        let charging = DeviceState {
            charge: Charge::Charging,
            ..asleep
        };
        assert_eq!(r.observe(&[charging], T0 + 43 * MIN).len(), 1);
        // Millivolts or the source alone are no change.
        let other = DeviceState {
            charge: Charge::Charging,
            presence: Presence::Asleep,
            millivolts: Some(3700),
            source: Source::Push,
            ..state(79, 6)
        };
        assert!(r.observe(&[other], T0 + 44 * MIN).is_empty());
        // Each device on its own; a clock set back still counts 30 min apart.
        let mut b = state(50, 1);
        b.key = DeviceKey("logi:4082:2".into());
        assert_eq!(r.observe(&[b.clone()], T0 + 45 * MIN).len(), 1);
        b.updated_ms = 2;
        assert_eq!(r.observe(&[b], T0 + 10 * MIN).len(), 1);
    }

    /// Discharging samples, one per `(hours after T0, percent)`.
    fn discharge(points: &[(u64, u8)]) -> Vec<Sample> {
        points
            .iter()
            .map(|&(h, p)| sample(T0 + h * HOUR, p))
            .collect()
    }

    #[test]
    fn time_left_needs_two_hours_and_three_points() {
        // Two points over 5 h.
        let two = discharge(&[(0, 100), (5, 90)]);
        assert_eq!(time_left(&two, T0 + 5 * HOUR), None);
        // Three points over less than 2 h.
        let mut short = discharge(&[(0, 100), (1, 98)]);
        short.push(sample(T0 + 2 * HOUR - MIN, 96));
        assert_eq!(time_left(&short, T0 + 2 * HOUR), None);
        // Three points over 2 h.
        let enough = discharge(&[(0, 100), (1, 98), (2, 96)]);
        assert!(time_left(&enough, T0 + 2 * HOUR).is_some());
        // An asleep stretch splits the span.
        let mut split = enough.clone();
        split[1].presence = Presence::Asleep;
        assert_eq!(time_left(&split, T0 + 2 * HOUR), None);
        // No samples, or no drop: no estimate.
        assert_eq!(time_left(&[], T0), None);
        assert_eq!(
            time_left(&discharge(&[(0, 90), (1, 90), (2, 90)]), T0 + 2 * HOUR),
            None
        );
    }

    #[test]
    fn time_left_ignores_charging_spans() {
        let charging = |h: u64, p| Sample {
            charge: Charge::Charging,
            ..sample(T0 + h * HOUR, p)
        };
        // A long charge followed by a short discharge.
        let mut s = vec![charging(0, 40), charging(2, 70), charging(4, 100)];
        s.extend(discharge(&[(5, 100), (6, 99)]));
        assert_eq!(time_left(&s, T0 + 6 * HOUR), None);
        // A charge in the middle splits two short discharges.
        let mut s = discharge(&[(0, 80), (1, 78)]);
        s.push(charging(2, 79));
        s.extend(discharge(&[(3, 79), (4, 77)]));
        assert_eq!(time_left(&s, T0 + 4 * HOUR), None);
        // A level that rose (charged unseen) starts a new span.
        let s = discharge(&[(0, 60), (1, 58), (2, 90), (3, 88)]);
        assert_eq!(time_left(&s, T0 + 3 * HOUR), None);
        // On the charger now: no estimate from the earlier discharge.
        let mut s = discharge(&[(0, 100), (2, 95), (5, 90)]);
        s.push(charging(6, 90));
        assert_eq!(time_left(&s, T0 + 6 * HOUR), None);
    }

    #[test]
    fn time_left_estimate() {
        // 100 → 90 % over 5 h: 2 points an hour, 45 h left at 90 %.
        let s = discharge(&[(0, 100), (2, 95), (5, 90)]);
        assert_eq!(
            time_left(&s, T0 + 5 * HOUR),
            Some(Duration::from_secs(45 * 3600))
        );
        // The latest qualifying span counts, at the latest level, even while asleep.
        let mut s = discharge(&[(0, 100), (1, 90), (2, 80)]);
        s.push(Sample {
            presence: Presence::Asleep,
            ..sample(T0 + 3 * HOUR, 80)
        });
        s.extend(discharge(&[(10, 80), (12, 78), (15, 75), (16, 75)]));
        s.push(Sample {
            presence: Presence::Asleep,
            ..sample(T0 + 17 * HOUR, 74)
        });
        // 80 → 75 over 6 h (the 16 h point extends the span): 74 % / (5 % per 6 h).
        let left = time_left(&s, T0 + 18 * HOUR).unwrap();
        assert_eq!(left, Duration::from_millis(74 * 6 * HOUR / 5));
        // Samples after `now` (a clock set back) are left out.
        assert_eq!(
            time_left(&s, T0 + 5 * HOUR),
            Some(Duration::from_millis(80 * 2 * HOUR / 20))
        );
    }

    #[test]
    fn add_keeps_time_order_and_30_days() {
        let mut other = sample(T0, 50);
        other.key = DeviceKey("logi:4082:2".into());
        let mut history = HashMap::new();
        let now = T0 + 31 * 24 * HOUR;
        add(
            &mut history,
            [
                sample(now - 2 * HOUR, 80),
                other,
                sample(now - 3 * HOUR, 81),
                sample(T0 + 12 * HOUR, 90),
            ],
            now,
        );
        assert_eq!(
            history[&DeviceKey("logi:4082:1".into())],
            [sample(now - 3 * HOUR, 81), sample(now - 2 * HOUR, 80)]
        );
        // A device whose samples are all older than 30 days is left out.
        assert!(!history.contains_key(&DeviceKey("logi:4082:2".into())));
        // Later adds drop what aged out since.
        let later = now + 30 * 24 * HOUR - HOUR;
        add(&mut history, [sample(later, 70)], later);
        assert_eq!(
            history[&DeviceKey("logi:4082:1".into())],
            [sample(later, 70)]
        );
    }

    #[test]
    fn time_left_text_rounds_to_hours() {
        assert_eq!(time_left_text(None), "Time left: not enough data");
        let h = |m: u64| Some(Duration::from_secs(m * 60));
        assert_eq!(time_left_text(h(45 * 60 + 10)), "Time left: about 45 h");
        assert_eq!(time_left_text(h(45 * 60 + 30)), "Time left: about 46 h");
        assert_eq!(time_left_text(h(20)), "Time left: less than 1 h");
    }

    fn key(k: &str) -> DeviceKey {
        DeviceKey(k.into())
    }

    #[test]
    fn session_break_round_trips() {
        let b = Sample::session_break(key("logi:4082:1"), T0);
        assert_eq!(
            b.to_line(),
            format!("{T0}\tlogi:4082:1\t-\tunknown\toffline\t-\tos")
        );
        assert_eq!(Sample::parse(&b.to_line()), Some(b.clone()));
        assert!(b.is_break());
        assert!(!sample(T0, 80).is_break());
        let offline = Sample {
            presence: Presence::Offline,
            ..sample(T0, 80)
        };
        assert!(!offline.is_break(), "an offline device that kept its level");
    }

    #[test]
    fn session_breaks_for_known_devices_once() {
        let mut r = Recorder::default();
        let mut c = state(60, 1);
        c.key = key("c");
        r.observe(&[c], T0);
        let a = Sample {
            key: key("a"),
            ..sample(T0, 80)
        };
        let mut history = HashMap::new();
        add(
            &mut history,
            [
                a,
                Sample {
                    key: key("b"),
                    ..sample(T0 - MIN, 50)
                },
                Sample::session_break(key("b"), T0),
            ],
            T0 + MIN,
        );
        // A device whose latest sample is a break already gets none; the recorder's too.
        let breaks = r.session_break(&history, T0 + MIN);
        assert_eq!(
            breaks,
            [
                Sample::session_break(key("a"), T0 + MIN),
                Sample::session_break(key("c"), T0 + MIN)
            ]
        );
        add(&mut history, breaks, T0 + MIN);
        assert!(r.session_break(&history, T0 + 2 * MIN).is_empty());
    }

    #[test]
    fn recording_after_a_break_waits_for_a_fresh_reading() {
        let mut r = Recorder::default();
        assert_eq!(r.observe(&[state(80, 1)], T0).len(), 1);
        let mut history = HashMap::new();
        add(&mut history, [sample(T0, 80)], T0);
        assert_eq!(r.session_break(&history, T0 + MIN).len(), 1);
        // The reading from before the break is not recorded again…
        assert!(r.observe(&[state(80, 1)], T0 + 2 * MIN).is_empty());
        // …a fresh one is, unchanged and within 30 min.
        let s = r.observe(&[state(80, 2)], T0 + 3 * MIN);
        assert_eq!(
            s.iter().map(|s| s.at_unix_ms).collect::<Vec<_>>(),
            [T0 + 3 * MIN]
        );
        // Then the usual rule again.
        assert!(r.observe(&[state(80, 3)], T0 + 4 * MIN).is_empty());
    }

    #[test]
    fn time_left_never_spans_a_session_break() {
        // 82 % at 22:00, 80 % at 23:00, the PC off overnight, 80 % at 08:00, 78 % at 09:00.
        let night = discharge(&[(22, 82), (23, 80), (32, 80), (33, 78)]);
        // Without a break: one 11 h span, about 214 h left.
        assert_eq!(
            time_left(&night, T0 + 33 * HOUR),
            Some(Duration::from_millis(78 * 11 * HOUR / 4))
        );
        let mut s = night.clone();
        s.insert(2, Sample::session_break(key("logi:4082:1"), T0 + 32 * HOUR));
        assert_eq!(time_left(&s, T0 + 33 * HOUR), None);
        // The span after the break alone, once long enough: 80 → 76 % over 2 h.
        s.extend(discharge(&[(34, 76)]));
        assert_eq!(
            time_left(&s, T0 + 34 * HOUR),
            Some(Duration::from_millis(76 * 2 * HOUR / 4))
        );
    }

    #[test]
    fn suspend_break_precedes_the_resume_reading() {
        let mut r = Recorder::default();
        let mut history = HashMap::new();
        add(&mut history, r.observe(&[state(80, 1)], T0), T0);
        // Suspend at 23:00: the break is recorded before the PC sleeps.
        let suspend = T0 + HOUR;
        let breaks = r.session_break(&history, suspend);
        add(&mut history, breaks, suspend);
        // The device's resume reading at 08:00, unchanged, is recorded after it.
        let resume = T0 + 10 * HOUR;
        let read = r.observe(&[state(80, 2)], resume);
        assert_eq!(read.len(), 1);
        add(&mut history, read, resume);
        let at: Vec<(u64, bool)> = history[&key("logi:4082:1")]
            .iter()
            .map(|s| (s.at_unix_ms, s.is_break()))
            .collect();
        assert_eq!(at, [(T0, false), (suspend, true), (resume, false)]);
        // Suspend again, then the resume break before any reading: it adds nothing.
        let breaks = r.session_break(&history, resume + HOUR);
        assert_eq!(breaks.len(), 1);
        add(&mut history, breaks, resume + HOUR);
        assert!(r.session_break(&history, resume + 2 * HOUR).is_empty());
    }

    #[test]
    fn time_left_skips_trailing_breaks() {
        let mut s = discharge(&[(0, 100), (2, 95), (5, 90)]);
        s.push(Sample {
            presence: Presence::Asleep,
            ..sample(T0 + 6 * HOUR, 90)
        });
        let before = time_left(&s, T0 + 6 * HOUR);
        assert_eq!(before, Some(Duration::from_secs(45 * 3600)));
        // The PC slept and woke; the device has not reported since.
        s.push(Sample::session_break(key("logi:4082:1"), T0 + 7 * HOUR));
        s.push(Sample::session_break(key("logi:4082:1"), T0 + 9 * HOUR));
        assert_eq!(time_left(&s, T0 + 9 * HOUR), before);
    }

    #[test]
    fn devices_with_only_breaks_are_dropped() {
        let mut history = HashMap::new();
        add(
            &mut history,
            [
                Sample::session_break(key("gone"), T0),
                Sample::session_break(key("gone"), T0 + HOUR),
                sample(T0, 80),
                Sample::session_break(key("logi:4082:1"), T0 + HOUR),
            ],
            T0 + HOUR,
        );
        assert!(!history.contains_key(&key("gone")));
        assert_eq!(history[&key("logi:4082:1")].len(), 2);
        // Once its readings age out, its breaks go too.
        let later = T0 + KEEP_MS + 1;
        add(&mut history, [], later);
        assert!(history.is_empty());
    }
}
