//! bounded reading history, kept ordered by sensor clock. it can be saved in
//! six hour slots, so one new reading only rewrites a small part of flash.

use nanodrip_core::{BACKFILL_GAP_SECONDS, BACKFILL_WINDOW_SECONDS};

/// seven days at the five minute sensor cadence.
const MAX_ENTRIES: usize = 2016;
/// live and backfilled copies of one measurement land this close together.
const DUPLICATE_WINDOW_SECONDS: u32 = 120;
const SLOT_SECONDS: u32 = 6 * 60 * 60;
/// eight days of slots, so a slot is never shared within `MAX_ENTRIES`.
pub const SLOTS: u16 = 32;
const RECORD_SIZE: usize = 13;
const NO_VALUE: u32 = 0;
const NO_TREND: i8 = 127;
const FLAG_BACKFILL: u8 = 1;
const TREND_SCALE: f32 = 10.0;

const READING_SECONDS: u32 = 300;
const MAX_READING_SPAN_SECONDS: u32 = 900;

type Record = [u8; RECORD_SIZE];

#[derive(Default)]
pub struct RangeTotals {
    pub above: u32,
    pub below: u32,
    pub within: u32,
    weighted_sum: u64,
}

impl RangeTotals {
    /// average glucose, weighted by how long each reading held.
    pub fn mean(&self) -> Option<f32> {
        let seconds = self.above + self.below + self.within;
        (seconds > 0).then(|| self.weighted_sum as f32 / seconds as f32)
    }
}

/// where an entry with this clock is saved.
pub fn slot_of(clock: u32) -> u16 {
    ((clock / SLOT_SECONDS) % u32::from(SLOTS)) as u16
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub clock: u32,
    pub time: Option<u64>,
    pub glucose: Option<u16>,
    pub state: u8,
    pub trend: Option<f32>,
    pub backfill: bool,
}

fn optional<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "null".to_string(), |v| v.to_string())
}

impl Entry {
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"clock":{},"time":{},"glucose":{},"state":{},"trend":{},"backfill":{}}}"#,
            self.clock,
            optional(self.time),
            optional(self.glucose),
            self.state,
            optional(self.trend),
            self.backfill,
        )
    }

    fn encode(&self) -> Record {
        let trend = self
            .trend
            .map_or(NO_TREND, |trend| (trend * TREND_SCALE).round() as i8);
        let mut record = [0u8; RECORD_SIZE];
        record[..4].copy_from_slice(&self.clock.to_le_bytes());
        record[4..8].copy_from_slice(&(self.time.unwrap_or(0) as u32).to_le_bytes());
        record[8..10].copy_from_slice(&self.glucose.unwrap_or(0).to_le_bytes());
        record[10] = self.state;
        record[11] = trend as u8;
        record[12] = u8::from(self.backfill);
        record
    }

    fn decode(record: &Record) -> Self {
        let word = |at: usize| {
            u32::from_le_bytes([record[at], record[at + 1], record[at + 2], record[at + 3]])
        };
        let glucose = u16::from_le_bytes([record[8], record[9]]);
        let trend = record[11] as i8;
        Self {
            clock: word(0),
            time: Some(word(4))
                .filter(|time| *time != NO_VALUE)
                .map(u64::from),
            glucose: Some(glucose).filter(|glucose| *glucose != 0),
            state: record[10],
            trend: (trend != NO_TREND).then_some(f32::from(trend) / TREND_SCALE),
            backfill: record[12] & FLAG_BACKFILL != 0,
        }
    }
}

fn record_clock(record: &Record) -> u32 {
    u32::from_le_bytes([record[0], record[1], record[2], record[3]])
}

/// holds packed records, because a week of decoded entries would take a large
/// share of the free memory on the device.
#[derive(Default)]
pub struct Store {
    records: Vec<Record>,
}

impl Store {
    /// adds an entry unless the same measurement is already held; a live
    /// reading replaces its backfilled twin.
    pub fn insert(&mut self, entry: Entry) -> bool {
        if self.records.capacity() == 0 {
            self.records.reserve_exact(MAX_ENTRIES + 1);
        }
        let at = self
            .records
            .partition_point(|r| record_clock(r) < entry.clock);
        let end = (at + 1).min(self.records.len());
        let near =
            |r: &&mut Record| record_clock(r).abs_diff(entry.clock) < DUPLICATE_WINDOW_SECONDS;
        if let Some(existing) = self.records[at.saturating_sub(1)..end]
            .iter_mut()
            .find(near)
        {
            let upgrade = Entry::decode(existing).backfill && !entry.backfill;
            if upgrade {
                *existing = entry.encode();
            }
            return upgrade;
        }
        self.records.insert(at, entry.encode());
        if self.records.len() > MAX_ENTRIES {
            self.records.remove(0);
        }
        true
    }

    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// everything held for one slot, ready to save.
    pub fn encode_slot(&self, slot: u16) -> Vec<u8> {
        let matching = self
            .records
            .iter()
            .filter(|r| slot_of(record_clock(r)) == slot);
        matching.flatten().copied().collect()
    }

    /// loads a saved slot. a damaged one is skipped, since losing some history
    /// beats showing readings that were not measured.
    pub fn restore_slot(&mut self, data: &[u8]) {
        let (records, rest) = data.as_chunks::<RECORD_SIZE>();
        if rest.is_empty() {
            for record in records {
                self.insert(Entry::decode(record));
            }
        }
    }

    pub fn latest(&self) -> Option<Entry> {
        self.records.last().map(Entry::decode)
    }

    /// the reading backfill should continue from: the one before the oldest
    /// gap the sensor can still fill. using this rather than the latest reading
    /// is what retries a backfill that a dropped connection cut short.
    pub fn resume_point(&self) -> Option<u32> {
        let latest = self.latest()?.clock;
        let reachable = latest.saturating_sub(BACKFILL_WINDOW_SECONDS);
        let oldest_gap = self
            .records
            .windows(2)
            .map(|pair| (record_clock(&pair[0]), record_clock(&pair[1])))
            .filter(|(_, after)| *after >= reachable)
            .find(|(before, after)| after - before > BACKFILL_GAP_SECONDS);
        Some(oldest_gap.map_or(latest, |(before, _)| before.max(reachable)))
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// the sensor clock range covering the last `seconds` of readings.
    pub fn recent_range(&self, seconds: u32) -> (u32, u32) {
        let latest = self.records.last().map_or(0, record_clock);
        (latest.saturating_sub(seconds), latest + READING_SECONDS)
    }

    /// time spent in each band between two sensor clock values. a reading
    /// holds until the next one, up to `MAX_READING_SPAN_SECONDS`, so an outage
    /// counts as unknown rather than as whatever came before it.
    pub fn range_totals(&self, start: u32, end: u32, low: u16, high: u16) -> RangeTotals {
        let mut totals = RangeTotals::default();
        let first = self.records.partition_point(|r| record_clock(r) < start);
        for (at, record) in self.records.iter().enumerate().skip(first) {
            let clock = record_clock(record);
            if clock >= end {
                break;
            }
            let next = self.records.get(at + 1).map(record_clock);
            let held = next.map_or(READING_SECONDS, |next| next - clock);
            let span = held.min(MAX_READING_SPAN_SECONDS).min(end - clock);
            let glucose = u16::from_le_bytes([record[8], record[9]]);
            match glucose {
                0 => continue,
                glucose if glucose < low => totals.below += span,
                glucose if glucose > high => totals.above += span,
                _ => totals.within += span,
            }
            totals.weighted_sum += u64::from(glucose) * u64::from(span);
        }
        totals
    }

    /// clock of the oldest of the newest `limit` entries.
    pub fn tail_start(&self, limit: usize) -> u32 {
        let start = self.records.len().saturating_sub(limit);
        self.records.get(start).map_or(u32::MAX, record_clock)
    }

    /// up to `count` entries from `clock` onward, so a reader can walk the
    /// history without holding the lock while it writes to a slow client.
    pub fn page(&self, clock: u32, count: usize) -> Vec<Entry> {
        let start = self.records.partition_point(|r| record_clock(r) < clock);
        let rest = self.records[start..].iter().take(count);
        rest.map(Entry::decode).collect()
    }
}
