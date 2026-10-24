//! listener-local playout continuity and bounded HUD_STATUS reports.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use crate::packet;
use crate::transport::Path;

const WINDOW: Duration = Duration::from_secs(2);
const MAX_OBSERVATIONS: usize = (WINDOW.as_millis() / crate::jitter::SLICE.as_millis()) as usize;
const MIN_OBSERVATIONS: usize = (500 / crate::jitter::SLICE.as_millis()) as usize;
const HISTORY_IDLE: Duration = Duration::from_secs(30);
const STALE: Duration = crate::jitter::IDLE_CLOSE;
const BATCH_INTERVAL: Duration = Duration::from_millis(500);
pub const MAX_RECORDS: usize = 32;

#[derive(Default)]
pub(crate) struct Continuity {
    samples: VecDeque<(usize, usize)>,
    last_playout: Option<Instant>,
}

impl Continuity {
    pub(crate) fn has_recent_playout(&self, now: Instant) -> bool {
        self.last_playout
            .is_some_and(|at| now.saturating_duration_since(at) < WINDOW)
    }

    pub(crate) fn record(&mut self, now: Instant, total: usize, impaired: usize) {
        // one observation is one rendered slice; discarded duration affects only the ratio.
        if self
            .last_playout
            .is_none_or(|at| now.saturating_duration_since(at) >= HISTORY_IDLE)
        {
            self.samples.clear();
        }
        if self.samples.len() == MAX_OBSERVATIONS {
            self.samples.pop_front();
        }
        self.samples.push_back((total, impaired.min(total)));
        self.last_playout = Some(now);
    }

    pub(crate) fn level(&self, now: Instant) -> u8 {
        if self
            .last_playout
            .is_none_or(|at| now.saturating_duration_since(at) >= STALE)
        {
            return 0;
        }
        let (total, impaired) = self
            .samples
            .iter()
            .fold((0, 0), |(total, impaired), (n, bad)| {
                (total + n, impaired + bad)
            });
        if total == 0 || self.samples.len() < MIN_OBSERVATIONS {
            0
        } else if impaired * 100 < total {
            3
        } else if impaired * 100 <= total * 5 {
            2
        } else {
            1
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub sender: u16,
    pub generation: u64,
    pub talkspurt: u16,
    pub level: u8,
    pub route: u8,
    pub delay_class: u8,
}

impl Status {
    pub fn selected_path(&mut self, path: Option<Path>) {
        self.route = match path {
            None | Some(Path::Game) => 0,
            Some(Path::Direct) => 1,
            Some(Path::Hub { .. }) => 2,
            Some(Path::PlayerRelay { .. }) => 3,
        };
    }
}

/// rejects an invalid batch rather than truncating or emitting an invalid datagram.
pub fn encode(records: &[Status]) -> Option<Vec<u8>> {
    if records.len() > MAX_RECORDS
        || records
            .iter()
            .any(|r| r.level > 3 || r.route > 3 || r.delay_class > 3)
    {
        return None;
    }
    let mut body = Vec::with_capacity(2 + 15 * records.len());
    body.extend_from_slice(&[1, records.len() as u8]);
    for record in records {
        body.extend_from_slice(&record.sender.to_be_bytes());
        body.extend_from_slice(&record.generation.to_be_bytes());
        body.extend_from_slice(&record.talkspurt.to_be_bytes());
        body.extend_from_slice(&[record.level, record.route, record.delay_class]);
    }
    Some(packet::encode(0x0a, 0, &body))
}

#[derive(Default)]
pub struct Reporter {
    sent: BTreeMap<u16, (Status, Instant)>,
    last_batch: Option<Instant>,
}

impl Reporter {
    /// changed records and two-second heartbeats share one global two-hertz budget.
    /// oldest reports go first so a busy sender cannot starve a batch beyond 32.
    pub fn batch(&mut self, active: &[Status], now: Instant) -> Option<Vec<u8>> {
        self.sent
            .retain(|sender, _| active.iter().any(|r| r.sender == *sender));
        if self
            .last_batch
            .is_some_and(|at| now.saturating_duration_since(at) < BATCH_INTERVAL)
        {
            return None;
        }
        let mut due: Vec<_> = active
            .iter()
            .filter(|record| {
                self.sent.get(&record.sender).is_none_or(|(last, at)| {
                    last != *record || now.saturating_duration_since(*at) >= WINDOW
                })
            })
            .copied()
            .collect();
        due.sort_by_key(|record| self.sent.get(&record.sender).map(|(_, at)| *at));
        due.truncate(MAX_RECORDS);
        if due.is_empty() {
            return None;
        }
        let packet = encode(&due)?;
        for record in due {
            self.sent.insert(record.sender, (record, now));
        }
        self.last_batch = Some(now);
        Some(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(sender: u16) -> Status {
        Status {
            sender,
            generation: 0x0102030405060708,
            talkspurt: 7,
            level: 3,
            route: 1,
            delay_class: 1,
        }
    }

    #[test]
    fn exact_vector_and_bounds() {
        assert_eq!(
            encode(&[status(2)]).unwrap(),
            [
                0x52, 1, 0x0a, 0, 0, 1, 1, 0, 2, 1, 2, 3, 4, 5, 6, 7, 8, 0, 7, 3, 1, 1
            ]
        );
        assert_eq!(encode(&[status(2); 32]).unwrap().len(), 487);
        assert!(encode(&[status(2); 33]).is_none());
        for invalid in [
            Status {
                level: 4,
                ..status(2)
            },
            Status {
                route: 4,
                ..status(2)
            },
            Status {
                delay_class: 4,
                ..status(2)
            },
        ] {
            assert!(encode(&[invalid]).is_none());
        }
        assert_eq!(encode(&[]).unwrap(), [0x52, 1, 0x0a, 0, 0, 1, 0]);
    }

    #[test]
    fn warmup_thresholds_rolling_window_and_stale() {
        let start = Instant::now();
        for (bad, expected) in [(0, 3), (1, 3), (2, 2), (10, 2), (11, 1), (200, 1)] {
            let mut quality = Continuity::default();
            assert_eq!(quality.level(start), 0);
            for n in 0..=200 {
                quality.record(
                    start + Duration::from_millis(n * 10),
                    100,
                    if n > 0 && n <= bad { 100 } else { 0 },
                );
            }
            assert_eq!(quality.level(start + WINDOW), expected, "{bad}/200");
            assert_eq!(quality.level(start + WINDOW + STALE), 0);
            for n in 201..=400 {
                quality.record(start + Duration::from_millis(n * 10), 100, 0);
            }
            assert_eq!(quality.level(start + WINDOW * 2), 3);
            quality.record(start + WINDOW * 2 + HISTORY_IDLE, 100, 0);
            assert_eq!(quality.level(start + WINDOW * 2 + HISTORY_IDLE), 0);
        }
    }

    #[test]
    fn warmup_requires_500_ms_of_measured_playout() {
        let start = Instant::now();
        let mut quality = Continuity::default();
        for n in 0..100 {
            let now = start + crate::jitter::SLICE * n;
            quality.record(now, crate::mix::SLICE_SAMPLES, 0);
            if n == 39 {
                assert_eq!(quality.level(now), 0, "400 ms is not enough");
            }
            if n == 49 {
                assert_eq!(quality.level(now), 3, "500 ms reaches a measured level");
            }
        }
        assert_eq!(quality.level(start + Duration::from_secs(1)), 3);
        let mut sparse = Continuity::default();
        for n in 0..20 {
            let now = start + Duration::from_millis(100) * n;
            sparse.record(now, crate::mix::SLICE_SAMPLES, 0);
        }
        assert_eq!(
            sparse.level(start + Duration::from_millis(1900)),
            0,
            "wall time does not warm up sparse observations"
        );
    }

    #[test]
    fn measured_window_excludes_idle_and_discard_volume_does_not_warm_it() {
        let start = Instant::now();
        let mut quality = Continuity::default();
        quality.record(
            start,
            crate::audio::OPUS_RATE as usize,
            crate::audio::OPUS_RATE as usize,
        );
        assert_eq!(
            quality.level(start),
            0,
            "discarded duration is not rendered duration"
        );
        for n in 1..50 {
            quality.record(
                start + crate::jitter::SLICE * n,
                crate::mix::SLICE_SAMPLES,
                0,
            );
        }
        assert_eq!(quality.level(start + crate::jitter::SLICE * 49), 1);
        for n in 50..=200 {
            quality.record(
                start + crate::jitter::SLICE * n,
                crate::mix::SLICE_SAMPLES,
                0,
            );
        }
        assert_eq!(
            quality.level(start + WINDOW),
            3,
            "discarded impairment leaves the bounded window"
        );
        let mut quality = Continuity::default();
        for n in 0..50 {
            quality.record(
                start + crate::jitter::SLICE * n,
                crate::mix::SLICE_SAMPLES,
                0,
            );
        }
        let resumed = start + Duration::from_secs(5);
        assert_eq!(
            quality.level(resumed),
            0,
            "idle history is not a live report"
        );
        assert!(!quality.has_recent_playout(resumed));
        quality.record(resumed, crate::mix::SLICE_SAMPLES, 0);
        assert_eq!(
            quality.level(resumed),
            3,
            "first fresh observation restores retained history"
        );
        for n in 1..=200 {
            quality.record(
                resumed + crate::jitter::SLICE * n,
                crate::mix::SLICE_SAMPLES,
                crate::mix::SLICE_SAMPLES,
            );
        }
        assert_eq!(quality.samples.len(), MAX_OBSERVATIONS);
        assert!(quality.samples.iter().all(|(n, bad)| n == bad));
        assert_eq!(
            quality.level(resumed + WINDOW),
            1,
            "old clean samples leave after two seconds of measured audio"
        );
    }

    #[test]
    fn changes_rate_limit_heartbeat_and_stop() {
        let start = Instant::now();
        let mut reporter = Reporter::default();
        let mut record = status(2);
        assert!(reporter.batch(&[record], start).is_some());
        record.level = 1;
        assert!(
            reporter
                .batch(&[record], start + Duration::from_millis(499))
                .is_none()
        );
        assert!(reporter.batch(&[record], start + BATCH_INTERVAL).is_some());
        assert!(reporter.batch(&[record], start + WINDOW).is_none());
        assert!(
            reporter
                .batch(&[record], start + WINDOW + BATCH_INTERVAL)
                .is_some()
        );
        assert!(reporter.batch(&[], start + WINDOW * 2).is_none());
        assert!(reporter.sent.is_empty());
        record.talkspurt += 1;
        assert!(reporter.batch(&[record], start + WINDOW * 2).is_some());
    }

    #[test]
    fn batching_is_fair_under_continuous_changes() {
        let start = Instant::now();
        let mut reporter = Reporter::default();
        let mut active: Vec<_> = (1..=70).map(status).collect();
        let mut seen = std::collections::BTreeSet::new();
        for n in 0..3 {
            for record in &mut active {
                record.level = n + 1;
            }
            let batch = reporter
                .batch(&active, start + BATCH_INTERVAL * u32::from(n))
                .unwrap();
            assert!(batch[6] <= 32);
            for record in batch[7..].as_chunks::<15>().0 {
                seen.insert(u16::from_be_bytes([record[0], record[1]]));
            }
        }
        assert_eq!(seen.len(), 70);
    }

    #[test]
    fn route_is_selected_path_independent_of_level_and_delay() {
        let mut record = status(2);
        record.delay_class = 0;
        for (path, route) in [
            (Path::Game, 0),
            (Path::Direct, 1),
            (Path::Hub { session: 1 }, 2),
            (
                Path::PlayerRelay {
                    player: 3,
                    session: 1,
                },
                3,
            ),
        ] {
            record.selected_path(Some(path));
            assert_eq!(record.route, route);
            assert_eq!(record.delay_class, 0);
        }
    }
}
