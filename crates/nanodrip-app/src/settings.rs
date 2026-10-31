//! pairing settings entered through the web ui. the ble side compares
//! `generation` between connections to notice that the sensor changed.

use nanodrip_core::Config;

/// matches the length the session requires of each certificate part.
const MIN_PART_LEN: usize = 101;
const MAX_SSID_LEN: usize = 32;
/// wpa2 passphrase limits; an empty password means an open network.
const WIFI_PASS_LENS: std::ops::RangeInclusive<usize> = 8..=63;
const DEFAULT_TARGET: (u16, u16) = (63, 140);
/// what dexcom sensors report, in mg/dL.
const GLUCOSE_LIMITS: std::ops::RangeInclusive<u16> = 40..=400;

#[derive(Clone, PartialEq)]
pub struct MqttConfig {
    pub address: String,
    pub user: Option<String>,
    pub password: Option<String>,
}

/// accepts `host:port`, which is all a connection needs.
fn valid_broker(address: &str) -> bool {
    let Some((host, port)) = address.rsplit_once(':') else {
        return false;
    };
    let host_ok = !host.is_empty() && !host.contains(char::is_whitespace);
    host_ok && port.parse::<u16>().is_ok_and(|port| port > 0)
}

fn present(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

#[derive(Clone, Default, PartialEq)]
pub struct Settings {
    pub code: Option<String>,
    pub part_a: Vec<u8>,
    pub part_b: Vec<u8>,
    pub part_c: Vec<u8>,
    pub generation: u32,
    pub wifi_ssid: Option<String>,
    pub wifi_pass: String,
    /// unset until a range is chosen, so the default can apply.
    pub target_range: Option<(u16, u16)>,
    pub mqtt: Option<MqttConfig>,
}

/// reads pasted hex; anything but hex digits is skipped.
pub fn decode_part(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .chars()
        .filter_map(|c| c.to_digit(16))
        .map(|d| d as u8)
        .collect();
    let (pairs, _) = digits.as_chunks::<2>();
    pairs.iter().map(|[high, low]| high << 4 | low).collect()
}

impl Settings {
    pub fn paired(&self) -> bool {
        self.code.is_some()
    }

    pub fn has_certificates(&self) -> bool {
        [&self.part_a, &self.part_b, &self.part_c]
            .iter()
            .all(|part| part.len() >= MIN_PART_LEN)
    }

    /// starts over with a new sensor; returns false for an unusable code.
    pub fn pair(&mut self, code: &str) -> bool {
        if Config::new(code).is_none() {
            return false;
        }
        self.code = Some(code.to_string());
        self.generation += 1;
        true
    }

    pub fn forget(&mut self) {
        self.code = None;
        self.generation += 1;
    }

    /// stores all three parts together, or none of them.
    pub fn set_certificates(&mut self, p1: &str, p2: &str, p3: &str) -> bool {
        let candidate = Self {
            part_a: decode_part(p1),
            part_b: decode_part(p2),
            part_c: decode_part(p3),
            ..self.clone()
        };
        let valid = candidate.has_certificates();
        if valid {
            *self = candidate;
        }
        valid
    }

    /// points the feed at a broker; an empty address turns it off.
    pub fn set_mqtt(&mut self, address: &str, user: &str, password: &str) -> bool {
        if address.is_empty() {
            self.mqtt = None;
            return true;
        }
        if !valid_broker(address) {
            return false;
        }
        self.mqtt = Some(MqttConfig {
            address: address.to_string(),
            user: present(user),
            password: present(password),
        });
        true
    }

    pub fn target(&self) -> (u16, u16) {
        self.target_range.unwrap_or(DEFAULT_TARGET)
    }

    pub fn set_target(&mut self, low: &str, high: &str) -> bool {
        let (Ok(low), Ok(high)) = (low.parse::<u16>(), high.parse::<u16>()) else {
            return false;
        };
        let valid = low < high && GLUCOSE_LIMITS.contains(&low) && GLUCOSE_LIMITS.contains(&high);
        if valid {
            self.target_range = Some((low, high));
        }
        valid
    }

    /// changes the network without touching the sensor pairing.
    pub fn set_wifi(&mut self, ssid: &str, password: &str) -> bool {
        let valid = !ssid.is_empty()
            && ssid.len() <= MAX_SSID_LEN
            && (password.is_empty() || WIFI_PASS_LENS.contains(&password.len()));
        if valid {
            self.wifi_ssid = Some(ssid.to_string());
            self.wifi_pass = password.to_string();
        }
        valid
    }

    /// session configuration for the paired sensor.
    pub fn config(&self) -> Option<Config> {
        let mut config = Config::new(self.code.as_deref()?)?;
        config.part_a = self.part_a.clone();
        config.part_b = self.part_b.clone();
        config.part_c = self.part_c.clone();
        Some(config)
    }
}
