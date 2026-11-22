use crate::documents::{self, Provenance};
use crate::entries::EntryKind;
use crate::ledger::Ledger;
use crate::repo::Repo;
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Days, Local, NaiveDate, TimeDelta, Utc};
use serde::Serialize;
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Clone, Copy, Default)]
pub(crate) struct TimeFilter {
    start: Option<Boundary>,
    end: Option<Boundary>,
}

#[derive(Clone, Copy)]
enum Boundary {
    Instant(DateTime<Utc>),
    Day(NaiveDate),
}

impl Boundary {
    fn cmp_event(self, timestamp: DateTime<Utc>) -> std::cmp::Ordering {
        match self {
            Self::Instant(start) => timestamp.cmp(&start),
            Self::Day(start) => timestamp.with_timezone(&Local).date_naive().cmp(&start),
        }
    }

    fn parse(value: &str, end: bool) -> Result<Self> {
        if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
            return Ok(Self::Day(if end {
                date.succ_opt().context("end date is out of range")?
            } else {
                date
            }));
        }
        Ok(Self::Instant(parse_timestamp(value).context(
            "expected a date (YYYY-MM-DD) or an RFC 3339 timestamp with timezone",
        )?))
    }
}

pub(crate) fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.to_utc())
}

pub(crate) fn duration(value: &str) -> Result<TimeDelta> {
    let split = value
        .trim()
        .find(|ch: char| !ch.is_ascii_digit())
        .context("expected a positive duration such as 30m, 2h or 7d")?;
    let (number, unit) = value.trim().split_at(split);
    let factor = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => bail!("duration unit must be s, m, h, d or w"),
    };
    number
        .parse::<i64>()
        .ok()
        .filter(|number| *number > 0)
        .and_then(|number| number.checked_mul(factor))
        .and_then(TimeDelta::try_seconds)
        .context("duration must be positive and within range")
}

impl TimeFilter {
    pub fn day(date: NaiveDate) -> Result<Self> {
        Ok(Self {
            start: Some(Boundary::Day(date)),
            end: Some(Boundary::Day(
                date.succ_opt().context("date is out of range")?,
            )),
        })
    }

    pub fn yesterday(today: NaiveDate) -> Result<Self> {
        Self::day(
            today
                .checked_sub_days(Days::new(1))
                .context("date is out of range")?,
        )
    }

    pub fn since(now: DateTime<Utc>, value: &str) -> Result<Self> {
        Ok(Self {
            start: Some(Boundary::Instant(
                now.checked_sub_signed(duration(value)?)
                    .context("start time is out of range")?,
            )),
            end: Some(Boundary::Instant(now)),
        })
    }

    pub fn between(from: Option<&str>, to: Option<&str>) -> Result<Self> {
        let start = from
            .map(|value| Boundary::parse(value, false))
            .transpose()?;
        let end = to.map(|value| Boundary::parse(value, true)).transpose()?;
        if let (Some(start), Some(end)) = (start, end) {
            let ordered = match (start, end) {
                (Boundary::Instant(a), Boundary::Instant(b)) => a < b,
                (Boundary::Day(a), Boundary::Day(b)) => a < b,
                (Boundary::Instant(a), Boundary::Day(b)) => {
                    a.with_timezone(&Local).date_naive() < b
                }
                (Boundary::Day(a), Boundary::Instant(b)) => {
                    a <= b.with_timezone(&Local).date_naive()
                }
            };
            ensure!(ordered, "the start of the time range must precede its end");
        }
        Ok(Self { start, end })
    }

    pub fn restricted(self) -> bool {
        self.start.is_some() || self.end.is_some()
    }

    pub fn includes(self, timestamp: Option<DateTime<Utc>>) -> bool {
        if !self.restricted() {
            return true;
        }
        timestamp.is_some_and(|at| {
            self.start.is_none_or(|start| start.cmp_event(at).is_ge())
                && self.end.is_none_or(|end| end.cmp_event(at).is_lt())
        })
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum EventKind {
    Memory,
    GitCommit,
    Consolidation,
}

impl EventKind {
    pub fn includes(self, kind: Option<EntryKind>) -> bool {
        let expected = match self {
            Self::Memory => EntryKind::Memory,
            Self::GitCommit => EntryKind::GitCommit,
            Self::Consolidation => EntryKind::Consolidation,
        };
        kind == Some(expected)
    }
}

#[derive(Serialize)]
pub(crate) struct Event {
    #[serde(flatten)]
    pub source: Provenance,
    pub local_day: NaiveDate,
    pub title: String,
    pub excerpt: String,
}

pub(crate) struct DatedEvent {
    pub at: DateTime<Utc>,
    pub event: Event,
}

pub(crate) fn events(
    repo: &Repo,
    ledger: &Ledger,
    raw: bool,
    time: TimeFilter,
    kind: Option<EventKind>,
) -> Result<Vec<DatedEvent>> {
    let archives = ledger.archives()?;
    let mut events = Vec::new();
    for document in documents::memories(repo, ledger)? {
        let mut source = document.provenance;
        let Some(at) = source.timestamp.as_deref().and_then(parse_timestamp) else {
            continue;
        };
        let id = Uuid::parse_str(&source.id)?;
        if (!raw && ledger.hidden(&archives, id))
            || !time.includes(Some(at))
            || kind.is_some_and(|kind| !kind.includes(source.entry_kind))
        {
            continue;
        }
        source.visibility = Some(ledger.visibility(&archives, id));
        let title = document
            .text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .trim_start_matches('#')
            .trim();
        let excerpt = document
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        events.push(DatedEvent {
            at,
            event: Event {
                source,
                local_day: at.with_timezone(&Local).date_naive(),
                title: shorten(title, 120),
                excerpt: shorten(&excerpt, 240),
            },
        });
    }
    events.sort_by(|a, b| {
        a.at.cmp(&b.at)
            .then_with(|| a.event.source.id.cmp(&b.event.source.id))
    });
    Ok(events)
}

fn shorten(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.into(),
    }
}

#[derive(Serialize)]
pub(crate) struct NeighborReport {
    pub window_seconds: i64,
    pub activities: Vec<Neighbor>,
}

#[derive(Serialize)]
pub(crate) struct Neighbor {
    #[serde(flatten)]
    pub event: Event,
    pub anchors: Vec<Relation>,
}

#[derive(Serialize)]
pub(crate) struct Relation {
    pub anchor_id: String,
    pub delta_millis: i64,
}

pub(crate) fn neighbors(
    events: Vec<DatedEvent>,
    anchors: &[(String, DateTime<Utc>)],
    window: TimeDelta,
    limit: usize,
) -> NeighborReport {
    let ids: BTreeSet<_> = anchors.iter().map(|(id, _)| id.as_str()).collect();
    let mut times: Vec<_> = anchors.iter().map(|(_, at)| *at).collect();
    times.sort_unstable();
    let mut nearby: Vec<_> = events
        .into_iter()
        .filter_map(|event| {
            if ids.contains(event.event.source.id.as_str()) {
                return None;
            }
            let pos = times.partition_point(|at| *at < event.at);
            let before = pos.checked_sub(1).and_then(|index| times.get(index));
            let nearest = before
                .into_iter()
                .chain(times.get(pos))
                .map(|at| (event.at - at).abs())
                .min()?;
            (nearest <= window).then_some((nearest, event))
        })
        .collect();
    nearby.sort_by(|(a, left), (b, right)| {
        a.cmp(b)
            .then_with(|| left.at.cmp(&right.at))
            .then_with(|| left.event.source.id.cmp(&right.event.source.id))
    });
    nearby.truncate(limit);
    NeighborReport {
        window_seconds: window.num_seconds(),
        activities: nearby
            .into_iter()
            .map(|(_, event)| {
                let anchors = anchors
                    .iter()
                    .filter_map(|(id, at)| {
                        let delta = event.at - at;
                        (delta.abs() <= window).then(|| Relation {
                            anchor_id: id.clone(),
                            delta_millis: delta.num_milliseconds(),
                        })
                    })
                    .collect();
                Neighbor {
                    event: event.event,
                    anchors,
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_original_instants_and_checks_ranges() -> Result<()> {
        let instant = |value| parse_timestamp(value).context("fixture time");
        let time = TimeFilter::between(
            Some("2026-02-01T10:30:00+02:00"),
            Some("2026-02-01T09:00:00Z"),
        )?;
        assert!(!time.includes(None));
        assert!(TimeFilter::default().includes(None));
        assert!(!time.includes(Some(instant("2026-02-01T08:29:59Z")?)));
        assert!(time.includes(Some(instant("2026-02-01T08:30:00Z")?)));
        assert!(!time.includes(Some(instant("2026-02-01T09:00:00Z")?)));
        assert!(TimeFilter::between(Some("2026-02-03"), Some("2026-02-02")).is_err());
        assert!(TimeFilter::between(Some("2026-02-30"), None).is_err());
        let now = instant("2026-02-02T12:00:00Z")?;
        let since = TimeFilter::since(now, "1d")?;
        assert!(since.includes(Some(instant("2026-02-01T14:00:00+02:00")?)));
        assert!(!since.includes(Some(now)));
        for invalid in ["0h", "-1h", "100", "7days", "999999999999999999w"] {
            assert!(duration(invalid).is_err());
        }
        Ok(())
    }
}
