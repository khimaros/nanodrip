//! readings on flash: a ring of sectors, each belonging to one sensor and named
//! by its header, so a raw copy can be read on its own. every sensor appends to
//! its own open sector, so sensors taking turns waste nothing. appending never
//! rewrites, which spreads wear over the whole ring, and a reading that was
//! replaced is simply superseded by a later record.

use std::io;

use crate::feed::Logged;
use crate::store::{decode_trend, encode_trend, Entry, SensorId, SensorName, Store};

pub const SECTOR_SIZE: usize = 4096;
const MAGIC: &[u8; 4] = b"NDLG";
const VERSION: u8 = 1;
const HEADER_SIZE: usize = 40;
const RECORD_SIZE: usize = 9;
const RECORDS_PER_SECTOR: usize = (SECTOR_SIZE - HEADER_SIZE) / RECORD_SIZE;
const ERASED: u8 = 0xFF;
const BACKFILL_FLAG: u16 = 1 << 15;
const CHECK_SEED: u8 = 0x5A;
const HEADER_SEED: u32 = 0x4E44_4C47;

/// a span of flash addressed from its own start, erased a sector at a time.
pub trait Flash {
    fn sectors(&self) -> u32;
    fn read(&self, at: u32, data: &mut [u8]) -> io::Result<()>;
    fn write(&mut self, at: u32, data: &[u8]) -> io::Result<()>;
    fn erase(&mut self, sector: u32) -> io::Result<()>;
}

#[derive(Clone, Copy, PartialEq)]
struct Tag {
    id: SensorId,
    name: SensorName,
    base: u32,
}

fn header_check(header: &[u8]) -> u32 {
    let fold = |hash: u32, byte: &u8| hash.rotate_left(5) ^ u32::from(*byte);
    header[..HEADER_SIZE - 4].iter().fold(HEADER_SEED, fold)
}

fn word(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn encode_header(seq: u32, tag: &Tag) -> [u8; HEADER_SIZE] {
    let mut header = [ERASED; HEADER_SIZE];
    header[..4].copy_from_slice(MAGIC);
    header[4] = VERSION;
    header[8..12].copy_from_slice(&seq.to_le_bytes());
    header[12..18].copy_from_slice(&tag.id);
    header[18..26].copy_from_slice(&tag.name);
    header[26..30].copy_from_slice(&tag.base.to_le_bytes());
    let check = header_check(&header);
    header[HEADER_SIZE - 4..].copy_from_slice(&check.to_le_bytes());
    header
}

fn decode_header(data: &[u8]) -> Option<(u32, Tag)> {
    let valid = data[..4] == *MAGIC
        && data[4] == VERSION
        && word(data, HEADER_SIZE - 4) == header_check(data);
    valid.then(|| {
        let mut tag = Tag {
            id: [0; 6],
            name: [0; 8],
            base: word(data, 26),
        };
        tag.id.copy_from_slice(&data[12..18]);
        tag.name.copy_from_slice(&data[18..26]);
        (word(data, 8), tag)
    })
}

fn record_check(record: &[u8]) -> u8 {
    record[..RECORD_SIZE - 1]
        .iter()
        .fold(CHECK_SEED, |sum, byte| sum ^ byte)
}

fn encode_record(entry: &Entry) -> [u8; RECORD_SIZE] {
    let flag = if entry.backfill { BACKFILL_FLAG } else { 0 };
    let glucose = entry.glucose.unwrap_or(0) | flag;
    let mut record = [0u8; RECORD_SIZE];
    record[..4].copy_from_slice(&entry.clock.to_le_bytes());
    record[4..6].copy_from_slice(&glucose.to_le_bytes());
    record[6] = entry.state;
    record[7] = encode_trend(entry.trend) as u8;
    record[8] = record_check(&record);
    record
}

fn decode_record(record: &[u8], id: SensorId) -> Option<Entry> {
    if record[RECORD_SIZE - 1] != record_check(record) {
        return None;
    }
    let glucose = u16::from_le_bytes([record[4], record[5]]);
    Some(Entry {
        sensor: id,
        clock: word(record, 0),
        time: None,
        glucose: Some(glucose & !BACKFILL_FLAG).filter(|glucose| *glucose != 0),
        state: record[6],
        trend: decode_trend(record[7] as i8),
        backfill: glucose & BACKFILL_FLAG != 0,
    })
}

struct Open {
    sector: u32,
    used: usize,
    tag: Tag,
}

pub struct Log<F: Flash> {
    flash: F,
    open: Vec<Open>,
    newest: u32,
    seq: u32,
}

impl<F: Flash> Log<F> {
    pub fn new(flash: F) -> Self {
        let newest = flash.sectors() - 1;
        Self {
            flash,
            open: Vec::new(),
            newest,
            seq: 0,
        }
    }

    /// how many readings the ring holds before it overwrites.
    pub fn capacity(&self) -> usize {
        self.flash.sectors() as usize * RECORDS_PER_SECTOR
    }

    fn sector_at(sector: u32) -> u32 {
        sector * SECTOR_SIZE as u32
    }

    /// every sector with a valid header, oldest first.
    fn scan(&self) -> io::Result<Vec<(u32, u32, Tag)>> {
        let mut header = [0u8; HEADER_SIZE];
        let mut found = Vec::new();
        for sector in 0..self.flash.sectors() {
            self.flash.read(Self::sector_at(sector), &mut header)?;
            if let Some((seq, tag)) = decode_header(&header) {
                found.push((seq, sector, tag));
            }
        }
        found.sort_by_key(|(seq, ..)| *seq);
        Ok(found)
    }

    /// loads a sector, returning how many record positions it has used.
    fn load_sector(&self, sector: u32, tag: &Tag, store: &mut Store) -> io::Result<usize> {
        let mut data = vec![0u8; SECTOR_SIZE];
        self.flash.read(Self::sector_at(sector), &mut data)?;
        store.identify(tag.id, tag.name, tag.base);
        let (records, _) = data[HEADER_SIZE..].as_chunks::<RECORD_SIZE>();
        let used = records
            .iter()
            .position(|record| record.iter().all(|byte| *byte == ERASED))
            .unwrap_or(RECORDS_PER_SECTOR);
        for entry in records[..used]
            .iter()
            .filter_map(|r| decode_record(r, tag.id))
        {
            store.insert(&entry);
        }
        Ok(used)
    }

    /// rebuilds the store and finds where each sensor left off.
    pub fn load(&mut self, store: &mut Store) -> io::Result<()> {
        for (seq, sector, tag) in self.scan()? {
            let used = self.load_sector(sector, &tag, store)?;
            self.open.retain(|open| open.tag.id != tag.id);
            self.open.push(Open { sector, used, tag });
            self.newest = sector;
            self.seq = seq;
        }
        Ok(())
    }

    /// takes the next sector in the ring, leaving other sensors' open ones.
    fn claim(&mut self, tag: Tag) -> io::Result<()> {
        let sectors = self.flash.sectors();
        let in_use = |sector: u32| self.open.iter().any(|open| open.sector == sector);
        let next = (1..=sectors)
            .map(|step| (self.newest + step) % sectors)
            .find(|sector| !in_use(*sector))
            .unwrap_or((self.newest + 1) % sectors);
        self.flash.erase(next)?;
        self.seq += 1;
        self.flash
            .write(Self::sector_at(next), &encode_header(self.seq, &tag))?;
        self.newest = next;
        self.open
            .retain(|open| open.tag.id != tag.id && open.sector != next);
        self.open.push(Open {
            sector: next,
            used: 0,
            tag,
        });
        Ok(())
    }

    fn append(&mut self, tag: Tag, entry: &Entry) -> io::Result<()> {
        let open = self.open.iter().find(|open| open.tag == tag);
        if open.is_none_or(|open| open.used == RECORDS_PER_SECTOR) {
            self.claim(tag)?;
        }
        let Some(open) = self.open.iter_mut().find(|open| open.tag == tag) else {
            return Ok(());
        };
        let at = Self::sector_at(open.sector) + (HEADER_SIZE + open.used * RECORD_SIZE) as u32;
        open.used += 1;
        self.flash.write(at, &encode_record(entry))
    }

    pub fn save(&mut self, changes: Vec<Logged>) -> io::Result<()> {
        for change in changes {
            let tag = Tag {
                id: change.entry.sensor,
                name: change.name,
                base: change.base,
            };
            self.append(tag, &change.entry)?;
        }
        Ok(())
    }
}
