const CONFIG_VARS: [&str; 7] = [
    "NANODRIP_DEVICE",
    "NANODRIP_WIFI_SSID",
    "NANODRIP_WIFI_PASS",
    "NANODRIP_AP_PASS",
    "NANODRIP_MQTT",
    "NANODRIP_MQTT_USER",
    "NANODRIP_MQTT_PASS",
];

fn main() {
    for name in CONFIG_VARS {
        println!("cargo:rerun-if-env-changed={name}");
    }
    embuild::espidf::sysenv::output();
}
