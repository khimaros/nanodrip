//! what must survive a reboot: pairing state in nvs, readings in the log.

use std::ffi::CStr;
use std::io;

use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use esp_idf_svc::sys::{
    esp, esp_err_t, esp_partition_erase_range, esp_partition_find_first, esp_partition_read,
    esp_partition_subtype_t, esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_DATA_UNDEFINED,
    esp_partition_t, esp_partition_type_t_ESP_PARTITION_TYPE_DATA, esp_partition_write,
};
use nanodrip_app::{Flash, MqttConfig, Settings, SECTOR_SIZE};

const NAMESPACE: &str = "nanodrip";
const MAX_BLOB: usize = 4096;
const KEY_SIZE: usize = 16;
const CODE: &str = "code";
const PART_A: &str = "part_a";
const PART_B: &str = "part_b";
const PART_C: &str = "part_c";
const KEY: &str = "key";
const ADDRESS: &str = "addr";
const WIFI_SSID: &str = "wifi_ssid";
const WIFI_PASS: &str = "wifi_pass";
const TARGET_RANGE: &str = "target";
const MQTT_ADDRESS: &str = "mqtt_addr";
const MQTT_USER: &str = "mqtt_user";
const MQTT_PASS: &str = "mqtt_pass";

pub const LOG_PARTITION: &CStr = c"log";
pub const TAG_PARTITION: &CStr = c"tags";
const LOG_SUBTYPE: esp_partition_subtype_t =
    esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_DATA_UNDEFINED;

/// the raw flash behind a log. nvs would need RAM for every page.
pub struct LogFlash {
    partition: *const esp_partition_t,
}

// the pointer is to the partition table, which is never written or freed.
unsafe impl Send for LogFlash {}

fn checked(result: esp_err_t) -> io::Result<()> {
    esp!(result).map_err(io::Error::other)
}

impl LogFlash {
    pub fn find(name: &CStr) -> Result<Self> {
        let kind = esp_partition_type_t_ESP_PARTITION_TYPE_DATA;
        // the name lives as long as the call and the partition table for good
        let partition = unsafe { esp_partition_find_first(kind, LOG_SUBTYPE, name.as_ptr()) };
        if partition.is_null() {
            return Err(anyhow!("no {name:?} partition"));
        }
        Ok(Self { partition })
    }
}

impl Flash for LogFlash {
    fn sectors(&self) -> u32 {
        unsafe { (*self.partition).size / SECTOR_SIZE as u32 }
    }

    fn read(&self, at: u32, data: &mut [u8]) -> io::Result<()> {
        let target = data.as_mut_ptr().cast();
        let size = data.len();
        checked(unsafe { esp_partition_read(self.partition, at as usize, target, size) })
    }

    fn write(&mut self, at: u32, data: &[u8]) -> io::Result<()> {
        let source = data.as_ptr().cast();
        let size = data.len();
        checked(unsafe { esp_partition_write(self.partition, at as usize, source, size) })
    }

    fn erase(&mut self, sector: u32) -> io::Result<()> {
        let at = sector as usize * SECTOR_SIZE;
        checked(unsafe { esp_partition_erase_range(self.partition, at, SECTOR_SIZE) })
    }
}

pub struct Persist {
    nvs: EspNvs<NvsDefault>,
}

impl Persist {
    pub fn new(partition: EspDefaultNvsPartition) -> Result<Self> {
        Ok(Self {
            nvs: EspNvs::new(partition, NAMESPACE, true)?,
        })
    }

    fn blob(&self, name: &str) -> Vec<u8> {
        let mut buffer = vec![0u8; MAX_BLOB];
        match self.nvs.get_blob(name, &mut buffer) {
            Ok(Some(value)) => value.to_vec(),
            _ => Vec::new(),
        }
    }

    fn text(&self, name: &str) -> Option<String> {
        String::from_utf8(self.blob(name))
            .ok()
            .filter(|text| !text.is_empty())
    }

    /// the build time values in `defaults` apply until the web ui replaces them.
    pub fn load_settings(&self, defaults: Settings) -> Settings {
        let (wifi_ssid, wifi_pass) = match self.text(WIFI_SSID) {
            Some(ssid) => (Some(ssid), self.text(WIFI_PASS).unwrap_or_default()),
            None => (defaults.wifi_ssid, defaults.wifi_pass),
        };
        let mqtt = self
            .text(MQTT_ADDRESS)
            .map(|address| MqttConfig {
                address,
                user: self.text(MQTT_USER),
                password: self.text(MQTT_PASS),
            })
            .or(defaults.mqtt);
        Settings {
            code: self.text(CODE),
            part_a: self.blob(PART_A),
            part_b: self.blob(PART_B),
            part_c: self.blob(PART_C),
            wifi_ssid,
            wifi_pass,
            target_range: self.target_range(),
            mqtt,
            ..Settings::default()
        }
    }

    pub fn save_settings(&mut self, settings: &Settings) -> Result<()> {
        let code = settings.code.as_deref().unwrap_or("");
        self.nvs.set_blob(CODE, code.as_bytes())?;
        self.nvs.set_blob(PART_A, &settings.part_a)?;
        self.nvs.set_blob(PART_B, &settings.part_b)?;
        self.nvs.set_blob(PART_C, &settings.part_c)?;
        let ssid = settings.wifi_ssid.as_deref().unwrap_or("");
        self.nvs.set_blob(WIFI_SSID, ssid.as_bytes())?;
        self.nvs
            .set_blob(WIFI_PASS, settings.wifi_pass.as_bytes())?;
        let mqtt = settings.mqtt.as_ref();
        let address = mqtt.map_or("", |mqtt| mqtt.address.as_str());
        let user = mqtt.and_then(|mqtt| mqtt.user.as_deref()).unwrap_or("");
        let password = mqtt.and_then(|mqtt| mqtt.password.as_deref()).unwrap_or("");
        self.nvs.set_blob(MQTT_ADDRESS, address.as_bytes())?;
        self.nvs.set_blob(MQTT_USER, user.as_bytes())?;
        self.nvs.set_blob(MQTT_PASS, password.as_bytes())?;
        if let Some((low, high)) = settings.target_range {
            let [low_a, low_b] = low.to_le_bytes();
            let [high_a, high_b] = high.to_le_bytes();
            self.nvs
                .set_blob(TARGET_RANGE, &[low_a, low_b, high_a, high_b])?;
        }
        Ok(())
    }

    fn target_range(&self) -> Option<(u16, u16)> {
        let [low_a, low_b, high_a, high_b]: [u8; 4] = self.blob(TARGET_RANGE).try_into().ok()?;
        Some((
            u16::from_le_bytes([low_a, low_b]),
            u16::from_le_bytes([high_a, high_b]),
        ))
    }

    pub fn key(&self) -> Option<[u8; KEY_SIZE]> {
        self.blob(KEY).try_into().ok()
    }

    pub fn set_key(&mut self, key: &[u8; KEY_SIZE]) -> Result<()> {
        self.nvs.set_blob(KEY, key)?;
        Ok(())
    }

    pub fn clear_key(&mut self) -> Result<()> {
        self.nvs.remove(KEY)?;
        Ok(())
    }

    /// the sensor this device paired with, so old sensors nearby are ignored.
    pub fn address(&self) -> Option<[u8; 6]> {
        self.blob(ADDRESS).try_into().ok()
    }

    pub fn set_address(&mut self, address: &[u8; 6]) -> Result<()> {
        self.nvs.set_blob(ADDRESS, address)?;
        Ok(())
    }

    pub fn clear_address(&mut self) -> Result<()> {
        self.nvs.remove(ADDRESS)?;
        Ok(())
    }
}
