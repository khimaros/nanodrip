//! reading history, mqtt feed and web ui, built on std::net so the same code
//! runs on the host and under esp-idf.

mod feed;
mod log;
mod mqtt;
mod settings;
mod store;
mod web;

pub use feed::{Feed, Logged, Shared};
pub use log::{Flash, Log, SECTOR_SIZE};
pub use settings::{MqttConfig, Settings};
pub use store::{Entry, SensorId, SensorName, Store};
pub use web::serve;
