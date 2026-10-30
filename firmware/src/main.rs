//! nanodrip firmware for the classic esp32: reads a dexcom g7/stelo sensor
//! over ble and serves the results over mqtt and http.

mod ble;
mod net;
mod persist;

use std::net::TcpListener;
use std::thread;

use anyhow::Result;
use esp32_nimble::BLEDevice;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sntp::EspSntp;
use nanodrip_app::{serve, Feed, MqttConfig, Settings};

use ble::Radio;
use net::Network;
use persist::{History, Persist};

/// make exports unset variables as empty, so empty has to mean unset here.
pub const fn configured(value: Option<&'static str>, default: &'static str) -> &'static str {
    match value {
        Some(value) if !value.is_empty() => value,
        _ => default,
    }
}

const DEVICE: &str = configured(option_env!("NANODRIP_DEVICE"), "esp32");
const MQTT_ADDRESS: Option<&str> = option_env!("NANODRIP_MQTT");
const MQTT_USER: Option<&str> = option_env!("NANODRIP_MQTT_USER");
const MQTT_PASS: Option<&str> = option_env!("NANODRIP_MQTT_PASS");
const DEFAULT_WIFI_SSID: Option<&str> = option_env!("NANODRIP_WIFI_SSID");
const DEFAULT_WIFI_PASS: &str = configured(option_env!("NANODRIP_WIFI_PASS"), "");
const HTTP_ADDRESS: &str = "0.0.0.0:80";
const WEB_STACK: usize = 8192;

fn mqtt_config() -> Option<MqttConfig> {
    let address = MQTT_ADDRESS.filter(|address| !address.is_empty())?;
    Some(MqttConfig {
        address: address.to_string(),
        user: MQTT_USER
            .filter(|user| !user.is_empty())
            .map(str::to_string),
        password: MQTT_PASS
            .filter(|pass| !pass.is_empty())
            .map(str::to_string),
    })
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let events = EspSystemEventLoop::take()?;
    let partition = EspDefaultNvsPartition::take()?;
    let persist = Persist::new(partition.clone())?;
    let saved = persist.load_settings(Settings {
        wifi_ssid: DEFAULT_WIFI_SSID
            .filter(|ssid| !ssid.is_empty())
            .map(str::to_string),
        wifi_pass: DEFAULT_WIFI_PASS.to_string(),
        mqtt: mqtt_config(),
        ..Settings::default()
    });
    let network = Network::start(
        peripherals.modem,
        events,
        partition,
        saved.wifi_ssid.as_deref(),
        &saved.wifi_pass,
    )?;
    let _sntp = EspSntp::new_default()?;

    let feed = Feed::new(DEVICE);
    *feed.shared().settings.lock().unwrap() = saved.clone();
    let history = History::new()?;
    history.restore(&feed);

    let listener = TcpListener::bind(HTTP_ADDRESS)?;
    let shared = feed.shared();
    thread::Builder::new()
        .stack_size(WEB_STACK)
        .spawn(move || serve(listener, shared))?;
    log::info!("nanodrip started as {DEVICE}");

    Radio {
        ble: BLEDevice::take(),
        feed,
        persist,
        history,
        network,
        generation: saved.generation,
        saved,
        sensor_address: [0; 6],
    }
    .run()
}
