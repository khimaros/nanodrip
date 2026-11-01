//! names put on moments in time. changes are appended to their own ring of
//! flash sectors and replayed at boot, so a tag is on flash before anyone is
//! told it was saved. the ring overwrites its oldest sector when full.

use std::io;

use crate::log::{word, Flash, SECTOR_SIZE};

pub const MAX_NAME: usize = 32;
const MAGIC: &[u8; 4] = b"NDTG";
const VERSION: u8 = 1;
const HEADER_SIZE: usize = 16;
const RECORD_SIZE: usize = 40;
const RECORDS_PER_SECTOR: usize = (SECTOR_SIZE - HEADER_SIZE) / RECORD_SIZE;
const NAME_AT: usize = 6;
const ERASED: u8 = 0xFF;
const CHECK_SEED: u8 = 0xA5;
const OP_ADD: u8 = 1;
const OP_REMOVE: u8 = 2;

#[derive(Clone, PartialEq)]
pub struct Tag {
    pub time: u32,
    pub name: String,
}

/// trims and lowercases a name, so one tag cannot split by spelling.
pub fn clean_name(name: &str) -> Option<String> {
    let clean = name.trim().to_lowercase();
    let valid = !clean.is_empty() && clean.len() <= MAX_NAME;
    (valid && !clean.chars().any(char::is_control)).then_some(clean)
}

fn check(data: &[u8]) -> u8 {
    data.iter().fold(CHECK_SEED, |sum, byte| sum ^ byte)
}

fn encode_header(seq: u32) -> [u8; HEADER_SIZE] {
    let mut header = [ERASED; HEADER_SIZE];
    header[..4].copy_from_slice(MAGIC);
    header[4] = VERSION;
    header[8..12].copy_from_slice(&seq.to_le_bytes());
    header[HEADER_SIZE - 1] = check(&header[..HEADER_SIZE - 1]);
    header
}

fn decode_header(data: &[u8]) -> Option<u32> {
    let valid = data[..4] == *MAGIC
        && data[4] == VERSION
        && data[HEADER_SIZE - 1] == check(&data[..HEADER_SIZE - 1]);
    valid.then(|| word(data, 8))
}

fn encode_record(op: u8, tag: &Tag) -> [u8; RECORD_SIZE] {
    let name = tag.name.as_bytes();
    let mut record = [0u8; RECORD_SIZE];
    record[0] = op;
    record[1] = name.len() as u8;
    record[2..NAME_AT].copy_from_slice(&tag.time.to_le_bytes());
    record[NAME_AT..NAME_AT + name.len()].copy_from_slice(name);
    record[RECORD_SIZE - 1] = check(&record[..RECORD_SIZE - 1]);
    record
}

fn decode_record(record: &[u8]) -> Option<(u8, Tag)> {
    if record[RECORD_SIZE - 1] != check(&record[..RECORD_SIZE - 1]) {
        return None;
    }
    let size = usize::from(record[1]).min(MAX_NAME);
    let name = std::str::from_utf8(&record[NAME_AT..NAME_AT + size]).ok()?;
    let name = name.to_string();
    Some((
        record[0],
        Tag {
            time: word(record, 2),
            name,
        },
    ))
}

/// the flash side of [`Tags`]: where the next change goes.
struct Ring {
    flash: Box<dyn Flash + Send>,
    newest: u32,
    used: usize,
    seq: u32,
}

impl Ring {
    fn sector_at(sector: u32) -> u32 {
        sector * SECTOR_SIZE as u32
    }

    /// every sector of ours, oldest first.
    fn scan(&self) -> io::Result<Vec<(u32, u32)>> {
        let mut header = [0u8; HEADER_SIZE];
        let mut found = Vec::new();
        for sector in 0..self.flash.sectors() {
            self.flash.read(Self::sector_at(sector), &mut header)?;
            if let Some(seq) = decode_header(&header) {
                found.push((seq, sector));
            }
        }
        found.sort_unstable();
        Ok(found)
    }

    /// claims the next sector. whatever it held may not be ours.
    fn advance(&mut self) -> io::Result<()> {
        let next = (self.newest + 1) % self.flash.sectors();
        self.flash.erase(next)?;
        self.seq += 1;
        self.flash
            .write(Self::sector_at(next), &encode_header(self.seq))?;
        self.newest = next;
        self.used = 0;
        Ok(())
    }

    fn append(&mut self, op: u8, tag: &Tag) -> io::Result<()> {
        if self.used == RECORDS_PER_SECTOR {
            self.advance()?;
        }
        let at = Self::sector_at(self.newest) + (HEADER_SIZE + self.used * RECORD_SIZE) as u32;
        self.used += 1;
        self.flash.write(at, &encode_record(op, tag))
    }
}

/// every tag, sorted by time. without flash they last until a restart.
#[derive(Default)]
pub struct Tags {
    held: Vec<Tag>,
    ring: Option<Ring>,
    revision: usize,
}

impl Tags {
    /// counts changes, so a page can tell its copy is out of date.
    pub fn revision(&self) -> usize {
        self.revision
    }

    pub fn page(&self, from: usize, count: usize) -> &[Tag] {
        let from = from.min(self.held.len());
        &self.held[from..self.held.len().min(from + count)]
    }

    fn position(&self, tag: &Tag) -> Option<usize> {
        self.held.iter().position(|held| held == tag)
    }

    fn hold(&mut self, tag: Tag) {
        let at = self.held.partition_point(|held| held.time <= tag.time);
        self.held.insert(at, tag);
    }

    fn replay(&mut self, op: u8, tag: Tag) {
        match (op, self.position(&tag)) {
            (OP_ADD, None) => self.hold(tag),
            (OP_REMOVE, Some(at)) => {
                self.held.remove(at);
            }
            _ => {}
        }
    }

    /// replays what `flash` holds and keeps later changes there.
    pub fn attach(&mut self, flash: Box<dyn Flash + Send>) -> io::Result<()> {
        let mut ring = Ring {
            newest: flash.sectors() - 1,
            used: RECORDS_PER_SECTOR,
            seq: 0,
            flash,
        };
        let mut data = vec![0u8; SECTOR_SIZE];
        for (seq, sector) in ring.scan()? {
            ring.flash.read(Ring::sector_at(sector), &mut data)?;
            let (records, _) = data[HEADER_SIZE..].as_chunks::<RECORD_SIZE>();
            let used = records
                .iter()
                .position(|record| record.iter().all(|byte| *byte == ERASED))
                .unwrap_or(RECORDS_PER_SECTOR);
            for (op, tag) in records[..used].iter().filter_map(|r| decode_record(r)) {
                self.replay(op, tag);
            }
            (ring.newest, ring.used, ring.seq) = (sector, used, seq);
        }
        self.ring = Some(ring);
        Ok(())
    }

    fn record(&mut self, op: u8, tag: &Tag) -> io::Result<()> {
        self.revision += 1;
        match self.ring.as_mut() {
            Some(ring) => ring.append(op, tag),
            None => Ok(()),
        }
    }

    pub fn add(&mut self, tag: Tag) -> io::Result<()> {
        if self.position(&tag).is_none() {
            self.record(OP_ADD, &tag)?;
            self.hold(tag);
        }
        Ok(())
    }

    /// reports whether there was such a tag.
    pub fn remove(&mut self, tag: &Tag) -> io::Result<bool> {
        let Some(at) = self.position(tag) else {
            return Ok(false);
        };
        self.record(OP_REMOVE, tag)?;
        self.held.remove(at);
        Ok(true)
    }

    /// reports whether there was a tag to rename.
    pub fn rename(&mut self, old: &Tag, name: String) -> io::Result<bool> {
        if self.position(old).is_none() {
            return Ok(false);
        }
        if old.name != name {
            self.add(Tag {
                time: old.time,
                name,
            })?;
            self.remove(old)?;
        }
        Ok(true)
    }
}
