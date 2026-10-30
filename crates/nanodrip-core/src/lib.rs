//! dexcom g7/stelo client protocol, written sans-io so that the esp32 firmware
//! and the host simulator run the same code. ble glue feeds `Session` events
//! and executes the `Action`s it returns.

#![no_std]

extern crate alloc;

mod jpake;
mod message;
mod session;

pub use message::{BackfillRecord, Reading};
pub use session::{
    Action, Channel, Config, Error, Session, BACKFILL_GAP_SECONDS, BACKFILL_WINDOW_SECONDS,
};
