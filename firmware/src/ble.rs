//! ble glue: finds the sensor, then runs a nanodrip-core session over gatt.
//! write pacing and subscription order follow xdrip, as sensors are picky.

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use bstr::ByteSlice;
use esp32_nimble::enums::{AuthReq, SecurityIOCap};
use esp32_nimble::utilities::BleUuid;
use esp32_nimble::{uuid128, BLEClient, BLEDevice, BLERemoteCharacteristic, BLEScan};
use esp_idf_svc::hal::task::block_on;
use nanodrip_app::{Feed, Settings};
use nanodrip_core::{Action, Channel, Session};
use rand_core::OsRng;

use crate::net::Network;
use crate::persist::{History, Persist};

const SERVICE: BleUuid = uuid128!("f8083532-849e-531c-c594-30f1f86a4ea5");
const CONTROL: BleUuid = uuid128!("f8083534-849e-531c-c594-30f1f86a4ea5");
const AUTH: BleUuid = uuid128!("f8083535-849e-531c-c594-30f1f86a4ea5");
const BACKFILL: BleUuid = uuid128!("f8083536-849e-531c-c594-30f1f86a4ea5");
const EXTRA: BleUuid = uuid128!("f8083538-849e-531c-c594-30f1f86a4ea5");
const NAME_PREFIXES: [&str; 3] = ["DXCM", "DX01", "DX02"];

/// bounds how long a change made in the web ui waits for a scan to end.
const SCAN_MS: i32 = 10_000;
const SCAN_INTERVAL_MS: u16 = 100;
const SCAN_WINDOW_MS: u16 = 99;
const CHUNK_GAP: Duration = Duration::from_millis(40);
const AFTER_DATA_GAP: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(250);
/// time allowed for the sensor to answer while authenticating.
const HANDSHAKE_IDLE: Duration = Duration::from_secs(15);
/// silence after a reading means the sensor has nothing more to send.
const DATA_IDLE: Duration = Duration::from_secs(3);
/// the sensor stops advertising for a while once it has been served.
const AFTER_SESSION: Duration = Duration::from_secs(60);
const UNPAIRED_WAIT: Duration = Duration::from_secs(5);

type Event = (Channel, Vec<u8>);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

struct Link {
    auth: BLERemoteCharacteristic,
    extra: BLERemoteCharacteristic,
    control: BLERemoteCharacteristic,
    backfill: BLERemoteCharacteristic,
    events: Sender<Event>,
    secured: bool,
    data_ready: bool,
    sent_extra: bool,
}

async fn subscribe(
    characteristic: &mut BLERemoteCharacteristic,
    channel: Channel,
    events: &Sender<Event>,
) -> Result<()> {
    let events = events.clone();
    log::info!("subscribe {channel:?}: {:?}", characteristic.properties());
    characteristic.on_notify(move |data| {
        let _ = events.send((channel, data.to_vec()));
    });
    // the mode is fixed per channel, as xdrip does it, even where both exist
    match channel {
        Channel::Auth | Channel::Control => characteristic.subscribe_indicate(true).await?,
        Channel::Extra | Channel::Backfill => characteristic.subscribe_notify(true).await?,
    }
    Ok(())
}

impl Link {
    async fn open(client: &mut BLEClient, events: Sender<Event>) -> Result<Self> {
        let service = client.get_service(SERVICE).await?;
        let auth = service.get_characteristic(AUTH).await?.clone();
        let extra = service.get_characteristic(EXTRA).await?.clone();
        let control = service.get_characteristic(CONTROL).await?.clone();
        let backfill = service.get_characteristic(BACKFILL).await?.clone();
        let mut link = Self {
            auth,
            extra,
            control,
            backfill,
            events,
            secured: false,
            data_ready: false,
            sent_extra: false,
        };
        subscribe(&mut link.extra, Channel::Extra, &link.events).await?;
        subscribe(&mut link.auth, Channel::Auth, &link.events).await?;
        Ok(link)
    }

    /// the data characteristics require an encrypted link. android restores
    /// that by itself on a bonded device; here it has to be asked for.
    async fn prepare_data(&mut self, client: &mut BLEClient) -> Result<()> {
        if !self.secured {
            client.secure_connection().await?;
            self.secured = true;
        }
        subscribe(&mut self.control, Channel::Control, &self.events).await?;
        subscribe(&mut self.backfill, Channel::Backfill, &self.events).await?;
        self.data_ready = true;
        Ok(())
    }

    async fn write(&mut self, client: &mut BLEClient, channel: Channel, data: &[u8]) -> Result<()> {
        log::info!("tx {channel:?} {} bytes opcode {:02x}", data.len(), data[0]);
        match channel {
            Channel::Extra => {
                self.extra.write_value(data, false).await?;
                self.sent_extra = true;
                sleep(CHUNK_GAP);
            }
            Channel::Auth => {
                if self.sent_extra {
                    sleep(AFTER_DATA_GAP);
                    self.sent_extra = false;
                }
                self.auth.write_value(data, true).await?;
            }
            Channel::Control => {
                if !self.data_ready {
                    self.prepare_data(client).await?;
                }
                self.control.write_value(data, true).await?;
            }
            Channel::Backfill => return Err(anyhow!("backfill is read only")),
        }
        Ok(())
    }
}

pub struct Radio {
    pub ble: &'static mut BLEDevice,
    pub feed: Feed,
    pub persist: Persist,
    pub history: History,
    pub network: Network,
    pub generation: u32,
    pub saved: Settings,
    pub sensor_address: [u8; 6],
}

impl Radio {
    fn configure(&mut self) {
        self.ble
            .security()
            .set_auth(AuthReq::Bond | AuthReq::Sc)
            .set_io_cap(SecurityIOCap::NoInputNoOutput);
    }

    /// a failed save loses nothing for now: the sensor can backfill the gap.
    fn save_history(&mut self) {
        let changes = self.feed.take_history_changes();
        if let Err(error) = self.history.save(changes) {
            log::error!("saving history failed: {error}");
        }
    }

    /// applies changes made through the web ui, returning the current settings.
    fn sync_settings(&mut self) -> Result<Settings> {
        let settings = self.feed.shared().settings.lock().unwrap().clone();
        if settings != self.saved {
            self.persist.save_settings(&settings)?;
            let wifi = (&settings.wifi_ssid, &settings.wifi_pass);
            if wifi != (&self.saved.wifi_ssid, &self.saved.wifi_pass) {
                self.network
                    .select(settings.wifi_ssid.as_deref(), &settings.wifi_pass)?;
            }
            self.saved = settings.clone();
        }
        if settings.generation != self.generation {
            self.generation = settings.generation;
            self.persist.clear_key()?;
            self.persist.clear_address()?;
            self.ble.delete_all_bonds()?;
            self.feed.reset_sensor();
            self.save_history();
        }
        Ok(settings)
    }

    async fn find_sensor(&mut self) -> Result<Option<esp32_nimble::BLEAddress>> {
        let known = self.persist.address();
        let mut scan = BLEScan::new();
        let found = scan
            .active_scan(true)
            .interval(SCAN_INTERVAL_MS)
            .window(SCAN_WINDOW_MS)
            .start(self.ble, SCAN_MS, |device, data| {
                let address = device.addr();
                if known.is_some() {
                    return (known == Some(address.as_be_bytes())).then_some(address);
                }
                let name = data.name()?;
                let matched = NAME_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with_str(prefix));
                if matched {
                    log::info!("advertising sensor: {name} at {address:?}");
                }
                matched.then_some(address)
            })
            .await?;
        Ok(found)
    }

    /// handles one action, returning follow-up actions and whether to stop.
    async fn apply(
        &mut self,
        action: Action,
        link: &mut Link,
        client: &mut BLEClient,
        session: &mut Session,
    ) -> Result<(Vec<Action>, bool)> {
        match action {
            Action::Write(channel, data) => link.write(client, channel, &data).await?,
            Action::RequestBond => {
                client.secure_connection().await?;
                link.secured = true;
                return Ok((session.on_bonded(), false));
            }
            Action::SaveKey(key) => {
                self.persist.set_key(&key)?;
                self.persist.set_address(&self.sensor_address)?;
            }
            Action::ClearKey => self.persist.clear_key()?,
            Action::Reading(reading) => {
                let entry = self.feed.record_reading(&reading, unix_now());
                log::info!("reading {}", entry.to_json());
            }
            Action::Backfill(record) => {
                self.feed.record_backfill(&record);
            }
            Action::Fail(error) => {
                log::warn!("session failed: {}", error.name());
                self.feed.set_error(Some(error.name()));
            }
            Action::Disconnect => return Ok((Vec::new(), true)),
        }
        Ok((Vec::new(), false))
    }

    async fn drive(
        &mut self,
        client: &mut BLEClient,
        link: &mut Link,
        session: &mut Session,
        events: &Receiver<Event>,
    ) -> Result<()> {
        let mut queue: VecDeque<Action> = session.on_connected(&mut OsRng).into();
        let mut last_activity = Instant::now();
        let mut idle_limit = HANDSHAKE_IDLE;
        while client.connected() {
            while let Some(action) = queue.pop_front() {
                if matches!(action, Action::Reading(_)) {
                    idle_limit = DATA_IDLE;
                }
                let (follow_up, stop) = self.apply(action, link, client, session).await?;
                if stop {
                    return Ok(());
                }
                queue.extend(follow_up);
            }
            match events.recv_timeout(POLL) {
                Ok((channel, data)) => {
                    let opcode = data.first().copied().unwrap_or(0);
                    if channel != Channel::Backfill {
                        log::info!("rx {channel:?} {} bytes opcode {opcode:02x}", data.len());
                    }
                    last_activity = Instant::now();
                    queue.extend(session.on_rx(&mut OsRng, channel, &data));
                }
                Err(RecvTimeoutError::Timeout) if last_activity.elapsed() > idle_limit => {
                    queue.extend(session.on_idle());
                }
                Err(_) => {}
            }
        }
        Err(anyhow!("sensor disconnected"))
    }

    async fn exchange(
        &mut self,
        address: &esp32_nimble::BLEAddress,
        session: &mut Session,
    ) -> Result<()> {
        self.sensor_address = address.as_be_bytes();
        let mut client = self.ble.new_client();
        client.connect(address).await?;
        let (sender, events) = channel();
        let mut link = Link::open(&mut client, sender).await?;
        let result = self.drive(&mut client, &mut link, session, &events).await;
        if client.connected() {
            let _ = client.disconnect();
        }
        result
    }

    async fn cycle(&mut self) -> Result<Duration> {
        let settings = self.sync_settings()?;
        let Some(mut config) = settings.config() else {
            self.feed.set_error(Some("unpaired"));
            return Ok(UNPAIRED_WAIT);
        };
        config.saved_key = self.persist.key();
        config.last_seen = self.feed.last_seen();
        let Some(address) = self.find_sensor().await? else {
            self.feed.set_error(Some("not-found"));
            return Ok(Duration::ZERO);
        };
        log::info!("sensor found");
        self.feed.set_error(None);
        let mut session = Session::new(config);
        if let Err(error) = self.exchange(&address, &mut session).await {
            log::warn!("session ended: {error}");
            self.feed.set_error(Some("ble"));
        }
        Ok(AFTER_SESSION)
    }

    /// serves sensors forever.
    pub fn run(mut self) -> ! {
        self.configure();
        loop {
            let wait = block_on(self.cycle()).unwrap_or_else(|error| {
                log::error!("cycle failed: {error}");
                UNPAIRED_WAIT
            });
            self.save_history();
            self.feed.flush();
            self.network.ensure();
            sleep(wait);
        }
    }
}
