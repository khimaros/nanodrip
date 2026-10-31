//! all readings, per sensor, in as little memory as they will fit. a sensor
//! measures on a fixed five minute grid, so a run of readings needs only its
//! first clock and three bytes each. anything off that grid starts a new run,
//! which keeps every clock exact.

use nanodrip_core::{BACKFILL_GAP_SECONDS, BACKFILL_WINDOW_SECONDS};

/// what fits in 60 KB, for boards with no PSRAM to measure.
const DEFAULT_CAPACITY: usize = 20_000;
/// live and backfilled copies of one measurement land this close together.
const DUPLICATE_WINDOW_SECONDS: u32 = 120;
const READING_SECONDS: u32 = 300;
const MAX_READING_SPAN_SECONDS: u32 = 900;
/// a week, so history is many small allocations rather than one that must be
/// contiguous and copied whenever it grows.
const MAX_RUN_CELLS: usize = 2016;
/// missing readings a run bridges with empty cells before a new one is cheaper.
const MAX_HOLE_CELLS: usize = 8;
const CELL_SIZE: usize = 3;
const EMPTY_CELL: Cell = [0xFF; CELL_SIZE];
const MAX_GLUCOSE: u16 = 510;
const GLUCOSE_BITS: u16 = 0x1FF;
const BACKFILL_BIT: u16 = 1 << 9;
const STATE_SHIFT: u16 = 10;
const MAX_STATES: usize = 64;
const NO_TREND: i8 = 127;
const TREND_SCALE: f32 = 10.0;
const NO_TIME: u32 = 0;

/// the BLE address, which is stable for the life of a sensor.
pub type SensorId = [u8; 6];
pub type SensorName = [u8; 8];
type Cell = [u8; CELL_SIZE];

pub fn hex_id(id: &SensorId) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn name_text(name: &SensorName) -> String {
    let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    String::from_utf8_lossy(&name[..end]).into_owned()
}

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

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub sensor: SensorId,
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
            r#"{{"sensor":"{}","clock":{},"time":{},"glucose":{},"state":{},"trend":{},"backfill":{}}}"#,
            hex_id(&self.sensor),
            self.clock,
            optional(self.time),
            optional(self.glucose),
            self.state,
            optional(self.trend),
            self.backfill,
        )
    }

    /// where this reading sits on the timeline shared by all sensors.
    pub fn instant(&self) -> u64 {
        self.time.unwrap_or(u64::from(self.clock))
    }
}

pub fn encode_trend(trend: Option<f32>) -> i8 {
    trend.map_or(NO_TREND, |trend| (trend * TREND_SCALE).round() as i8)
}

pub fn decode_trend(trend: i8) -> Option<f32> {
    (trend != NO_TREND).then_some(f32::from(trend) / TREND_SCALE)
}

/// a stretch of readings `READING_SECONDS` apart.
struct Run {
    start: u32,
    cells: Vec<Cell>,
}

impl Run {
    fn end(&self) -> u32 {
        self.start + self.cells.len() as u32 * READING_SECONDS
    }

    fn clock_at(&self, index: usize) -> u32 {
        self.start + index as u32 * READING_SECONDS
    }
}

/// one sensor's readings, on its own clock.
struct Sensor {
    id: SensorId,
    name: SensorName,
    /// wall clock at sensor clock zero, or `NO_TIME` until one is known.
    base: u32,
    runs: Vec<Run>,
    count: usize,
}

impl Sensor {
    fn iter_from(&self, clock: u32) -> impl Iterator<Item = (u32, Cell)> + '_ {
        let first = self.runs.partition_point(|run| run.end() <= clock);
        self.runs[first..].iter().flat_map(move |run| {
            let skip = clock.saturating_sub(run.start).div_ceil(READING_SECONDS) as usize;
            let cells = run.cells.iter().enumerate().skip(skip);
            cells
                .filter(|(_, cell)| **cell != EMPTY_CELL)
                .map(|(index, cell)| (run.clock_at(index), *cell))
        })
    }

    fn iter_rev(&self) -> impl Iterator<Item = (u32, Cell)> + '_ {
        self.runs.iter().rev().flat_map(|run| {
            let cells = run.cells.iter().enumerate().rev();
            cells
                .filter(|(_, cell)| **cell != EMPTY_CELL)
                .map(|(index, cell)| (run.clock_at(index), *cell))
        })
    }

    fn instant(&self, clock: u32) -> u64 {
        u64::from(self.base) + u64::from(clock)
    }

    fn first_clock(&self) -> Option<u32> {
        self.iter_from(0).next().map(|(clock, _)| clock)
    }

    fn last_clock(&self) -> Option<u32> {
        self.iter_rev().next().map(|(clock, _)| clock)
    }

    /// the held measurement close enough to `clock` to be the same one.
    fn twin(&self, clock: u32) -> Option<(u32, Cell)> {
        let at = self.runs.partition_point(|run| run.start <= clock);
        let near = self.runs[at.saturating_sub(1)..(at + 1).min(self.runs.len())].iter();
        near.flat_map(|run| {
            let floor = (clock.saturating_sub(run.start) / READING_SECONDS) as usize;
            let last = run.cells.len() - 1;
            [floor.min(last), (floor + 1).min(last)]
                .map(|index| (run.clock_at(index), run.cells[index]))
        })
        .find(|(held, cell)| *cell != EMPTY_CELL && held.abs_diff(clock) < DUPLICATE_WINDOW_SECONDS)
    }

    /// writes a cell into the run it belongs to, or starts one.
    fn place(&mut self, clock: u32, cell: Cell) {
        let at = self.runs.partition_point(|run| run.start <= clock);
        if at > 0 {
            let run = &mut self.runs[at - 1];
            let offset = clock - run.start;
            let index = (offset / READING_SECONDS) as usize;
            let on_grid = offset.is_multiple_of(READING_SECONDS);
            if on_grid && index < MAX_RUN_CELLS && index <= run.cells.len() + MAX_HOLE_CELLS {
                if index >= run.cells.len() {
                    run.cells.resize(index + 1, EMPTY_CELL);
                }
                run.cells[index] = cell;
                self.merge_next(at - 1);
                return;
            }
            if index + 1 < run.cells.len() {
                let start = run.clock_at(index + 1);
                let cells = run.cells.split_off(index + 1);
                self.runs.insert(at, Run { start, cells });
            }
        }
        let cells = vec![cell];
        self.runs.insert(
            at,
            Run {
                start: clock,
                cells,
            },
        );
        self.merge_next(at);
        if at > 0 {
            self.merge_next(at - 1);
        }
    }

    /// joins a run to the next one when they share a grid and nearly touch.
    fn merge_next(&mut self, at: usize) {
        let Some([run, next]) = self.runs.get_mut(at..at + 2) else {
            return;
        };
        let gap = next.start - run.start;
        let index = (gap / READING_SECONDS) as usize;
        let touching = (run.cells.len()..=run.cells.len() + MAX_HOLE_CELLS).contains(&index);
        let on_grid = gap.is_multiple_of(READING_SECONDS);
        if on_grid && touching && index + next.cells.len() <= MAX_RUN_CELLS {
            run.cells.resize(index, EMPTY_CELL);
            run.cells.append(&mut next.cells);
            self.runs.remove(at + 1);
        }
    }

    fn clear(&mut self, clock: u32) {
        let at = self.runs.partition_point(|run| run.start <= clock);
        let run = &mut self.runs[at - 1];
        let index = ((clock - run.start) / READING_SECONDS) as usize;
        run.cells[index] = EMPTY_CELL;
    }

    fn pop_oldest(&mut self) {
        while let Some(run) = self.runs.first_mut() {
            let cell = run.cells.remove(0);
            run.start += READING_SECONDS;
            if run.cells.is_empty() {
                self.runs.remove(0);
            }
            if cell != EMPTY_CELL {
                self.count -= 1;
                return;
            }
        }
    }
}

pub struct SensorInfo {
    pub id: SensorId,
    pub name: SensorName,
    pub base: u32,
    pub count: usize,
}

pub struct Store {
    sensors: Vec<Sensor>,
    /// state values seen, as there are too many possible ones for the cell.
    states: Vec<u8>,
    len: usize,
    capacity: usize,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            sensors: Vec::new(),
            states: Vec::new(),
            len: 0,
            capacity: DEFAULT_CAPACITY,
        }
    }
}

impl Store {
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn sensor(&self, id: &SensorId) -> Option<&Sensor> {
        self.sensors.iter().find(|sensor| sensor.id == *id)
    }

    fn sensor_mut(&mut self, id: SensorId) -> &mut Sensor {
        let at = self.sensors.iter().position(|sensor| sensor.id == id);
        let at = at.unwrap_or_else(|| {
            self.sensors.push(Sensor {
                id,
                name: [0; 8],
                base: NO_TIME,
                runs: Vec::new(),
                count: 0,
            });
            self.sensors.len() - 1
        });
        &mut self.sensors[at]
    }

    /// records what a session or the log knows about a sensor; a name or time
    /// already held is only replaced by a real one.
    pub fn identify(&mut self, id: SensorId, name: SensorName, base: u32) {
        let sensor = self.sensor_mut(id);
        if name != [0; 8] {
            sensor.name = name;
        }
        if base != NO_TIME && sensor.base == NO_TIME {
            sensor.base = base;
        }
    }

    pub fn info(&self, id: &SensorId) -> Option<SensorInfo> {
        self.sensor(id).map(Self::summary)
    }

    fn summary(sensor: &Sensor) -> SensorInfo {
        SensorInfo {
            id: sensor.id,
            name: sensor.name,
            base: sensor.base,
            count: sensor.count,
        }
    }

    pub fn sensors(&self) -> Vec<SensorInfo> {
        self.sensors.iter().map(Self::summary).collect()
    }

    fn state_code(&mut self, state: u8) -> u16 {
        let known = self.states.iter().position(|seen| *seen == state);
        let code = known.unwrap_or_else(|| {
            if self.states.len() < MAX_STATES {
                self.states.push(state);
            }
            self.states.len() - 1
        });
        code as u16
    }

    fn encode(&mut self, entry: &Entry) -> Cell {
        let glucose = entry.glucose.unwrap_or(0).min(MAX_GLUCOSE);
        let backfill = if entry.backfill { BACKFILL_BIT } else { 0 };
        let word = glucose | backfill | (self.state_code(entry.state) << STATE_SHIFT);
        let [low, high] = word.to_le_bytes();
        [low, high, encode_trend(entry.trend) as u8]
    }

    fn decode(&self, sensor: &Sensor, clock: u32, cell: Cell) -> Entry {
        let word = u16::from_le_bytes([cell[0], cell[1]]);
        let time = (sensor.base != NO_TIME).then(|| sensor.instant(clock));
        Entry {
            sensor: sensor.id,
            clock,
            time,
            glucose: Some(word & GLUCOSE_BITS).filter(|glucose| *glucose != 0),
            state: self.states[usize::from(word >> STATE_SHIFT)],
            trend: decode_trend(cell[2] as i8),
            backfill: word & BACKFILL_BIT != 0,
        }
    }

    /// adds an entry unless the same measurement is already held; a live
    /// reading replaces its backfilled twin.
    pub fn insert(&mut self, entry: &Entry) -> bool {
        let cell = self.encode(entry);
        let sensor = self.sensor_mut(entry.sensor);
        if let Some((held, twin)) = sensor.twin(entry.clock) {
            let upgrade =
                u16::from_le_bytes([twin[0], twin[1]]) & BACKFILL_BIT != 0 && !entry.backfill;
            if !upgrade {
                return false;
            }
            sensor.clear(held);
            sensor.count -= 1;
            self.len -= 1;
        }
        let sensor = self.sensor_mut(entry.sensor);
        sensor.place(entry.clock, cell);
        sensor.count += 1;
        self.len += 1;
        if self.len > self.capacity {
            self.evict();
        }
        true
    }

    /// drops the oldest reading of any sensor.
    fn evict(&mut self) {
        let first = |sensor: &&mut Sensor| {
            sensor
                .first_clock()
                .map_or(u64::MAX, |clock| sensor.instant(clock))
        };
        if let Some(sensor) = self.sensors.iter_mut().min_by_key(first) {
            sensor.pop_oldest();
            self.len -= 1;
        }
    }

    fn selected<'a>(&'a self, filter: Option<&'a SensorId>) -> impl Iterator<Item = &'a Sensor> {
        let all = self.sensors.iter();
        all.filter(move |sensor| filter.is_none_or(|id| sensor.id == *id))
    }

    pub fn latest(&self) -> Option<Entry> {
        let newest = self.sensors.iter().filter_map(|sensor| {
            let (clock, cell) = sensor.iter_rev().next()?;
            Some((sensor.instant(clock), sensor, clock, cell))
        });
        let (_, sensor, clock, cell) = newest.max_by_key(|found| found.0)?;
        Some(self.decode(sensor, clock, cell))
    }

    pub fn latest_clock(&self, id: &SensorId) -> Option<u32> {
        self.sensor(id)?.last_clock()
    }

    /// the oldest reading of a sensor, while there is still room for older ones.
    pub fn oldest(&self, id: &SensorId) -> Option<u32> {
        let room = self.len < self.capacity;
        self.sensor(id)?.first_clock().filter(|_| room)
    }

    /// the reading backfill should continue from: the one before the oldest
    /// gap the sensor can still fill. using this rather than the latest reading
    /// is what retries a backfill that a dropped connection cut short.
    pub fn resume_point(&self, id: &SensorId) -> Option<u32> {
        let sensor = self.sensor(id)?;
        let latest = sensor.last_clock()?;
        let reachable = latest.saturating_sub(BACKFILL_WINDOW_SECONDS);
        let mut oldest_gap = None;
        let mut after = latest;
        for (before, _) in sensor.iter_rev() {
            if after - before > BACKFILL_GAP_SECONDS {
                oldest_gap = Some(before);
            }
            if before < reachable {
                break;
            }
            after = before;
        }
        Some(oldest_gap.map_or(latest, |before| before.max(reachable)))
    }

    /// the timeline range covering the last `seconds` of readings.
    pub fn recent_range(&self, seconds: u32, filter: Option<&SensorId>) -> (u64, u64) {
        let newest = self.selected(filter).filter_map(|sensor| {
            let clock = sensor.last_clock()?;
            Some(sensor.instant(clock))
        });
        let latest = newest.max().unwrap_or(0);
        (
            latest.saturating_sub(u64::from(seconds)),
            latest + u64::from(READING_SECONDS),
        )
    }

    fn sensor_totals(
        sensor: &Sensor,
        range: (u64, u64),
        target: (u16, u16),
        totals: &mut RangeTotals,
    ) {
        let (start, end) = range;
        let first = start
            .saturating_sub(u64::from(sensor.base))
            .min(u64::from(u32::MAX));
        let mut cells = sensor.iter_from(first as u32).peekable();
        while let Some((clock, cell)) = cells.next() {
            let instant = sensor.instant(clock);
            if instant >= end {
                break;
            }
            let held = cells.peek().map_or(READING_SECONDS, |next| next.0 - clock);
            let span = u64::from(held.min(MAX_READING_SPAN_SECONDS)).min(end - instant) as u32;
            let glucose = u16::from_le_bytes([cell[0], cell[1]]) & GLUCOSE_BITS;
            match glucose {
                0 => continue,
                glucose if glucose < target.0 => totals.below += span,
                glucose if glucose > target.1 => totals.above += span,
                _ => totals.within += span,
            }
            totals.weighted_sum += u64::from(glucose) * u64::from(span);
        }
    }

    /// time spent in each band between two points on the timeline. a reading
    /// holds until the next one, up to `MAX_READING_SPAN_SECONDS`, so an outage
    /// counts as unknown rather than as whatever came before it.
    pub fn range_totals(
        &self,
        range: (u64, u64),
        target: (u16, u16),
        filter: Option<&SensorId>,
    ) -> RangeTotals {
        let mut totals = RangeTotals::default();
        for sensor in self.selected(filter) {
            Self::sensor_totals(sensor, range, target, &mut totals);
        }
        totals
    }

    /// where on the timeline the newest `limit` entries begin.
    pub fn tail_start(&self, limit: usize, filter: Option<&SensorId>) -> u64 {
        if limit >= self.len {
            return 0;
        }
        let mut instants: Vec<u64> = self
            .selected(filter)
            .flat_map(|sensor| {
                sensor
                    .iter_rev()
                    .take(limit)
                    .map(|(c, _)| sensor.instant(c))
            })
            .collect();
        instants.sort_unstable();
        instants
            .len()
            .checked_sub(limit)
            .map_or(0, |at| instants[at])
    }

    /// up to `count` entries from `start` onward, in time order across sensors,
    /// so a reader can walk the history without holding the lock while it
    /// writes to a slow client. ties stay together, so the next page can begin
    /// just after the last instant returned.
    pub fn page(&self, start: u64, count: usize, filter: Option<&SensorId>) -> Vec<Entry> {
        let mut page: Vec<Entry> = Vec::new();
        for sensor in self.selected(filter) {
            let first = start
                .saturating_sub(u64::from(sensor.base))
                .min(u64::from(u32::MAX));
            let cells = sensor.iter_from(first as u32).take(count + 1);
            page.extend(cells.map(|(clock, cell)| self.decode(sensor, clock, cell)));
        }
        // the stable sort borrows more stack than a connection thread has
        page.sort_unstable_by_key(Entry::instant);
        if let Some(last) = page.get(count.saturating_sub(1)).map(Entry::instant) {
            page.retain(|entry| entry.instant() <= last);
        }
        page
    }
}
