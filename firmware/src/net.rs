//! wifi bring-up. when no network is configured, or the configured one cannot
//! be joined at boot, the device hosts its own so the web ui stays reachable
//! and a mistyped password never locks anyone out.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, BlockingWifi, ClientConfiguration, Configuration,
    EspWifi, PmfConfiguration, ScanMethod, ScanSortMethod,
};

use crate::configured;

const AP_PASS: &str = configured(option_env!("NANODRIP_AP_PASS"), "nanodrip-setup");
const AP_SSID: &str = "nanodrip-setup";
const AP_CHANNEL: u8 = 6;
/// esp-idf does not retry a failed association by itself.
const JOIN_ATTEMPTS: u32 = 4;
const JOIN_RETRY_GAP: Duration = Duration::from_secs(2);

pub struct Network {
    wifi: BlockingWifi<EspWifi<'static>>,
    client: bool,
}

fn too_long<T>(_: T) -> anyhow::Error {
    anyhow!("wifi credential too long")
}

fn client_config(ssid: &str, password: &str) -> Result<Configuration> {
    let auth_method = if password.is_empty() {
        AuthMethod::None
    } else {
        AuthMethod::WPA2Personal
    };
    Ok(Configuration::Client(ClientConfiguration {
        ssid: ssid.try_into().map_err(too_long)?,
        password: password.try_into().map_err(too_long)?,
        auth_method,
        // a home network may have several access points; take the strongest
        scan_method: ScanMethod::CompleteScan(ScanSortMethod::Signal),
        // some routers refuse clients that do not offer this
        pmf_cfg: PmfConfiguration::Capable { required: false },
        ..Default::default()
    }))
}

fn access_point_config() -> Result<Configuration> {
    Ok(Configuration::AccessPoint(AccessPointConfiguration {
        ssid: AP_SSID.try_into().map_err(too_long)?,
        password: AP_PASS.try_into().map_err(too_long)?,
        auth_method: AuthMethod::WPA2Personal,
        channel: AP_CHANNEL,
        ..Default::default()
    }))
}

impl Network {
    pub fn start(
        modem: Modem<'static>,
        events: EspSystemEventLoop,
        nvs: EspDefaultNvsPartition,
        ssid: Option<&str>,
        password: &str,
    ) -> Result<Self> {
        let driver = EspWifi::new(modem, events.clone(), Some(nvs))?;
        let wifi = BlockingWifi::wrap(driver, events)?;
        let mut network = Self {
            wifi,
            client: false,
        };
        // the sensor link and history still work with no network at all
        if let Err(error) = network.select(ssid, password) {
            log::error!("no network available: {error}");
        }
        Ok(network)
    }

    fn host_setup_network(&mut self) -> Result<()> {
        self.client = false;
        self.wifi.set_configuration(&access_point_config()?)?;
        self.wifi.start()?;
        self.wifi.wait_netif_up()?;
        let ip = self.wifi.wifi().ap_netif().get_ip_info()?.ip;
        log::info!("hosting wifi network {AP_SSID}, web ui: http://{ip}/");
        Ok(())
    }

    /// reports how the target network looks from here, as the esp32 only
    /// hears 2.4 ghz and a join failure alone does not say why.
    fn log_visibility(&mut self, ssid: &str) {
        let Ok(found) = self.wifi.scan() else {
            log::warn!("wifi scan failed");
            return;
        };
        let matching: Vec<_> = found.iter().filter(|ap| ap.ssid.as_str() == ssid).collect();
        log::info!(
            "wifi scan: {} networks, {} match",
            found.len(),
            matching.len()
        );
        for ap in matching {
            log::info!(
                "wifi candidate: channel {} signal {} dbm auth {:?}",
                ap.channel,
                ap.signal_strength,
                ap.auth_method,
            );
        }
    }

    fn try_join(&mut self) -> Result<()> {
        self.wifi.connect()?;
        self.wifi.wait_netif_up()?;
        let ip = self.wifi.wifi().sta_netif().get_ip_info()?.ip;
        log::info!("wifi joined, web ui: http://{ip}/");
        Ok(())
    }

    fn join(&mut self) -> Result<()> {
        let mut outcome = Err(anyhow!("wifi was never attempted"));
        for attempt in 1..=JOIN_ATTEMPTS {
            outcome = self.try_join();
            let Err(error) = &outcome else { break };
            log::warn!("wifi join attempt {attempt}/{JOIN_ATTEMPTS} failed: {error}");
            let _ = self.wifi.disconnect();
            sleep(JOIN_RETRY_GAP);
        }
        outcome
    }

    /// moves to the given network, falling back to the setup network.
    pub fn select(&mut self, ssid: Option<&str>, password: &str) -> Result<()> {
        if self.wifi.is_started()? {
            self.wifi.stop()?;
        }
        let Some(ssid) = ssid else {
            return self.host_setup_network();
        };
        self.wifi
            .set_configuration(&client_config(ssid, password)?)?;
        self.wifi.start()?;
        log::info!(
            "joining wifi: ssid {} bytes, password {} bytes",
            ssid.len(),
            password.len()
        );
        self.log_visibility(ssid);
        match self.join() {
            Ok(()) => {
                self.client = true;
                Ok(())
            }
            Err(_) => {
                self.wifi.stop()?;
                self.host_setup_network()
            }
        }
    }

    /// rejoins after a dropped link; safe to call on every cycle.
    pub fn ensure(&mut self) {
        if !self.client || self.wifi.is_connected().unwrap_or(false) {
            return;
        }
        let _ = self.join();
    }
}
