//! reading history, mqtt feed and web ui, built on std::net so the same code
//! runs on the host and under esp-idf.

mod feed;
mod mqtt;
mod settings;
mod store;
mod web;

pub use feed::{Feed, HistoryChanges, Shared};
pub use settings::{MqttConfig, Settings};
pub use store::{Entry, Store, SLOTS as HISTORY_SLOTS};
pub use web::serve;
