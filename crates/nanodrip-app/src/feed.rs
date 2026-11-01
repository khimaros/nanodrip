//! turns session results into history and queued mqtt messages.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nanodrip_core::{BackfillRecord, Reading};

use crate::mqtt::{Connection, Message};
use crate::settings::{MqttConfig, Settings};
use crate::store::{Entry, SensorId, SensorName, Store};
use crate::tags::Tags;

/// one day at the five minute cadence; older unsent values are dropped.
const MAX_PENDING: usize = 288;
/// a wall clock before this has not been set yet.
pub(crate) const MIN_VALID_UNIX: u64 = 1_600_000_000;

/// state read by the web ui while the ble side keeps writing.
#[derive(Default)]
pub struct Shared {
    pub device: String,
    pub store: Mutex<Store>,
    pub tags: Mutex<Tags>,
    pub pending: AtomicUsize,
    pub free_memory: AtomicUsize,
    pub free_internal: AtomicUsize,
    pub settings: Mutex<Settings>,
    pub last_error: Mutex<Option<&'static str>>,
}

/// a reading the host has yet to save, with the sensor details stored with it.
pub struct Logged {
    pub entry: Entry,
    pub name: SensorName,
    pub base: u32,
}

pub struct Feed {
    shared: Arc<Shared>,
    queue: VecDeque<Message>,
    sensor: SensorId,
    unsaved: Vec<Entry>,
    /// the sensor had nothing before this reading. kept in memory only, since
    /// finding out again after a reboot costs one empty request.
    history_floor: Option<(SensorId, u32)>,
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
            sensor: [0; 6],
            unsaved: Vec::new(),
            history_floor: None,
        }
    }

    fn mqtt_config(&self) -> Option<MqttConfig> {
        self.shared.settings.lock().unwrap().mqtt.clone()
    }

    /// everything recorded since the last call, for the host to save.
    pub fn take_unsaved(&mut self) -> Vec<Logged> {
        let store = self.shared.store.lock().unwrap();
        let details = |entry: Entry| {
            let info = store.info(&entry.sensor);
            let (name, base) = info.map_or(([0; 8], 0), |info| (info.name, info.base));
            Logged { entry, name, base }
        };
        self.unsaved.drain(..).map(details).collect()
    }

    pub fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    pub fn set_error(&self, error: Option<&'static str>) {
        *self.shared.last_error.lock().unwrap() = error;
    }

    /// names the sensor the next readings come from.
    pub fn select_sensor(&mut self, id: SensorId, name: SensorName) {
        self.sensor = id;
        self.shared.store.lock().unwrap().identify(id, name, 0);
    }

    pub fn last_seen(&self) -> Option<u32> {
        self.shared.store.lock().unwrap().resume_point(&self.sensor)
    }

    /// the reading to look further back from, if that is still worth trying.
    pub fn oldest(&self) -> Option<u32> {
        let oldest = self.shared.store.lock().unwrap().oldest(&self.sensor);
        oldest.filter(|clock| Some((self.sensor, *clock)) != self.history_floor)
    }

    /// records that the sensor holds nothing older than what is stored now.
    pub fn history_exhausted(&mut self) {
        let oldest = self.shared.store.lock().unwrap().oldest(&self.sensor);
        self.history_floor = oldest.map(|clock| (self.sensor, clock));
    }

    fn time_of(&self, clock: u32) -> Option<u64> {
        let store = self.shared.store.lock().unwrap();
        let base = store.info(&self.sensor).map_or(0, |info| info.base);
        (base != 0).then(|| u64::from(base) + u64::from(clock))
    }

    /// stores an entry, reporting whether it was new.
    fn store(&mut self, entry: &Entry) -> bool {
        let added = self.shared.store.lock().unwrap().insert(entry);
        if added {
            self.unsaved.push(entry.clone());
        }
        added
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
            let base = (now - u64::from(reading.clock)) as u32;
            let mut store = self.shared.store.lock().unwrap();
            store.identify(self.sensor, [0; 8], base);
        }
        let clock = reading.measured_at();
        let entry = Entry {
            sensor: self.sensor,
            clock,
            time: self.time_of(clock),
            glucose: reading.glucose,
            state: reading.state,
            trend: reading.trend,
            backfill: false,
        };
        if self.store(&entry) {
            self.enqueue(&entry, "glucose", true);
        }
        entry
    }

    pub fn record_backfill(&mut self, record: &BackfillRecord) -> Entry {
        let entry = Entry {
            sensor: self.sensor,
            clock: record.clock,
            time: self.time_of(record.clock),
            glucose: Some(record.glucose),
            state: record.state,
            trend: record.trend,
            backfill: true,
        };
        if self.store(&entry) {
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
