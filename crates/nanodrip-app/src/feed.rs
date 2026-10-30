//! turns session results into history and queued mqtt messages.

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nanodrip_core::{BackfillRecord, Reading};

use crate::mqtt::{Connection, Message};
use crate::settings::{MqttConfig, Settings};
use crate::store::{slot_of, Entry, Store};

/// one day at the five minute cadence; older unsent values are dropped.
const MAX_PENDING: usize = 288;
/// a wall clock before this has not been set yet.
const MIN_VALID_UNIX: u64 = 1_600_000_000;

/// state read by the web ui while the ble side keeps writing.
#[derive(Default)]
pub struct Shared {
    pub device: String,
    pub store: Mutex<Store>,
    pub pending: AtomicUsize,
    pub settings: Mutex<Settings>,
    pub last_error: Mutex<Option<&'static str>>,
}

/// what a host must do to its saved history to match the store.
#[derive(Default)]
pub struct HistoryChanges {
    pub clear: bool,
    pub slots: Vec<(u16, Vec<u8>)>,
}

pub struct Feed {
    shared: Arc<Shared>,
    queue: VecDeque<Message>,
    clock_offset: Option<u64>,
    dirty_slots: BTreeSet<u16>,
    history_cleared: bool,
}

impl Feed {
    pub fn new(device: &str) -> Self {
        let shared = Shared {
            device: device.to_string(),
            ..Shared::default()
        };
        Self {
            shared: Arc::new(shared),
            queue: VecDeque::new(),
            clock_offset: None,
            dirty_slots: BTreeSet::new(),
            history_cleared: false,
        }
    }

    fn mqtt_config(&self) -> Option<MqttConfig> {
        self.shared.settings.lock().unwrap().mqtt.clone()
    }

    /// loads a slot saved by an earlier boot.
    pub fn restore_slot(&self, data: &[u8]) {
        self.shared.store.lock().unwrap().restore_slot(data);
    }

    /// everything that changed since the last call, for the host to save.
    pub fn take_history_changes(&mut self) -> HistoryChanges {
        let store = self.shared.store.lock().unwrap();
        let slots = self.dirty_slots.iter();
        let changes = HistoryChanges {
            clear: self.history_cleared,
            slots: slots
                .map(|slot| (*slot, store.encode_slot(*slot)))
                .collect(),
        };
        drop(store);
        self.dirty_slots.clear();
        self.history_cleared = false;
        changes
    }

    pub fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    pub fn set_error(&self, error: Option<&'static str>) {
        *self.shared.last_error.lock().unwrap() = error;
    }

    /// drops what belongs to the previous sensor: its clock started at a
    /// different moment, so its readings cannot share a timeline with the next.
    pub fn reset_sensor(&mut self) {
        self.shared.store.lock().unwrap().clear();
        self.clock_offset = None;
        self.dirty_slots.clear();
        self.history_cleared = true;
    }

    pub fn last_seen(&self) -> Option<u32> {
        self.shared.store.lock().unwrap().resume_point()
    }

    fn time_of(&self, clock: u32) -> Option<u64> {
        self.clock_offset.map(|offset| offset + u64::from(clock))
    }

    fn enqueue(&mut self, entry: &Entry, topic: &str, retain: bool) {
        if self.mqtt_config().is_none() {
            return;
        }
        if self.queue.len() == MAX_PENDING {
            self.queue.pop_front();
        }
        self.queue.push_back(Message {
            topic: format!("nanodrip/{}/{}", self.shared.device, topic),
            payload: entry.to_json(),
            retain,
        });
        self.shared
            .pending
            .store(self.queue.len(), Ordering::Relaxed);
    }

    /// records a live reading against the wall clock `now`.
    pub fn record_reading(&mut self, reading: &Reading, now: u64) -> Entry {
        if now >= MIN_VALID_UNIX {
            self.clock_offset = Some(now - u64::from(reading.clock));
        }
        let clock = reading.measured_at();
        let entry = Entry {
            clock,
            time: self.time_of(clock),
            glucose: reading.glucose,
            state: reading.state,
            trend: reading.trend,
            backfill: false,
        };
        if self.shared.store.lock().unwrap().insert(entry.clone()) {
            self.dirty_slots.insert(slot_of(entry.clock));
            self.enqueue(&entry, "glucose", true);
        }
        entry
    }

    pub fn record_backfill(&mut self, record: &BackfillRecord) -> Entry {
        let entry = Entry {
            clock: record.clock,
            time: self.time_of(record.clock),
            glucose: Some(record.glucose),
            state: record.state,
            trend: record.trend,
            backfill: true,
        };
        if self.shared.store.lock().unwrap().insert(entry.clone()) {
            self.dirty_slots.insert(slot_of(entry.clock));
            self.enqueue(&entry, "backfill", false);
        }
        entry
    }

    /// delivers queued messages in order. anything unconfirmed stays queued
    /// for the next flush, so a broker outage does not lose readings.
    pub fn flush(&mut self) {
        let Some(config) = self.mqtt_config() else {
            return;
        };
        if self.queue.is_empty() {
            return;
        }
        let client_id = format!("nanodrip-{}", self.shared.device);
        let opened = Connection::open(
            &config.address,
            &client_id,
            config.user.as_deref(),
            config.password.as_deref(),
        );
        if let Ok(mut connection) = opened {
            while let Some(message) = self.queue.front() {
                if connection.publish(message).is_err() {
                    break;
                }
                self.queue.pop_front();
            }
            connection.close();
        }
        self.shared
            .pending
            .store(self.queue.len(), Ordering::Relaxed);
    }
}
