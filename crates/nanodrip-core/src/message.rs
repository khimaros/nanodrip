//! wire layouts of the sensor messages nanodrip understands.

use aes::cipher::{BlockEncrypt, KeyInit};
use aes::{Aes128, Block};

pub const OP_AUTH_REQUEST: u8 = 0x02;
pub const OP_AUTH_CHALLENGE_TX: u8 = 0x04;
pub const OP_DISCONNECT: u8 = 0x09;
pub const OP_ROUND: u8 = 0x0a;
pub const OP_CERT_INFO: u8 = 0x0b;
pub const OP_SIGN_CHALLENGE: u8 = 0x0c;
pub const OP_GLUCOSE: u8 = 0x4e;
pub const OP_BACKFILL: u8 = 0x59;
const OP_AUTH_CHALLENGE_RX: u8 = 0x03;
const OP_AUTH_STATUS: u8 = 0x05;

pub const KEEP_ALIVE: [u8; 2] = [0x06, 0x19];
pub const BOND_ECHOES: [&[u8]; 3] = [&KEEP_ALIVE, &[0xff, 0x06, 0x01], &[0x06, 0x00]];
pub const CHALLENGE_OUT: [u8; 3] = [0x0d, 0x00, 0x02];
pub const LONG_GLUCOSE_REQUEST: [u8; 3] = [OP_GLUCOSE, 0x0a, 0xa9];

pub const AUTH_SLOT: u8 = 0x02;
pub const TOKEN_SIZE: usize = 8;
pub const SIGN_TOKEN_SIZE: usize = 16;
pub const BACKFILL_RECORD_SIZE: usize = 9;
const CHALLENGE_RX_SIZE: usize = 17;
const CERT_INFO_RX_SIZE: usize = 7;
const GLUCOSE_RX_SIZE: usize = 19;
const STATUS_RX_SIZE: usize = 3;
const STATUS_BONDED: u8 = 1;
const STATUS_REFRESH: u8 = 3;
const MAX_ERROR_GLUCOSE: u16 = 13;
const GLUCOSE_MASK: u16 = 0x0fff;
const TREND_UNKNOWN: i8 = 127;
const STATE_OK: u8 = 0x06;
const STATE_NEEDS_CALIBRATION: u8 = 0x07;

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// aes over the doubled input, truncated: the auth proof for both directions.
pub fn auth_hash(key: &[u8; 16], data: &[u8; TOKEN_SIZE]) -> [u8; TOKEN_SIZE] {
    let mut block = Block::default();
    block[..TOKEN_SIZE].copy_from_slice(data);
    block[TOKEN_SIZE..].copy_from_slice(data);
    Aes128::new(key.into()).encrypt_block(&mut block);
    let mut out = [0u8; TOKEN_SIZE];
    out.copy_from_slice(&block[..TOKEN_SIZE]);
    out
}

/// splits a challenge message into the sensor proof and the sensor challenge.
pub fn parse_challenge(data: &[u8]) -> Option<([u8; TOKEN_SIZE], [u8; TOKEN_SIZE])> {
    if data.len() != CHALLENGE_RX_SIZE || data[0] != OP_AUTH_CHALLENGE_RX {
        return None;
    }
    Some((data[1..9].try_into().ok()?, data[9..17].try_into().ok()?))
}

pub struct AuthStatus {
    pub authenticated: bool,
    pub bonded: bool,
    pub refresh: bool,
}

pub fn parse_status(data: &[u8]) -> Option<AuthStatus> {
    if data.len() < STATUS_RX_SIZE || data[0] != OP_AUTH_STATUS {
        return None;
    }
    Some(AuthStatus {
        authenticated: data[1] == 1,
        bonded: data[2] == STATUS_BONDED,
        refresh: data[2] == STATUS_REFRESH,
    })
}

/// returns the size of the certificate the sensor will send back.
pub fn parse_cert_info(data: &[u8]) -> Option<usize> {
    if data.len() != CERT_INFO_RX_SIZE || data[0] != OP_CERT_INFO || data[1] != 0 {
        return None;
    }
    Some(u16_at(data, 3) as usize).filter(|size| *size > 0)
}

/// a value is only trusted when the sensor state says it is, and values up to
/// 13 are error codes rather than measurements.
fn trusted_glucose(raw: u16, state: u8) -> Option<u16> {
    let glucose = raw & GLUCOSE_MASK;
    let usable = matches!(state, STATE_OK | STATE_NEEDS_CALIBRATION);
    (usable && glucose > MAX_ERROR_GLUCOSE).then_some(glucose)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub glucose: Option<u16>,
    pub state: u8,
    pub trend: Option<f32>,
    pub age: u16,
    pub clock: u32,
}

impl Reading {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < GLUCOSE_RX_SIZE || data[0] != OP_GLUCOSE {
            return None;
        }
        let state = data[14];
        let trend = data[15] as i8;
        Some(Self {
            glucose: trusted_glucose(u16_at(data, 12), state),
            state,
            trend: (trend != TREND_UNKNOWN).then_some(f32::from(trend) / 10.0),
            age: u16_at(data, 10),
            clock: u32_at(data, 2),
        })
    }

    /// sensor clock at the moment this value was measured.
    pub fn measured_at(&self) -> u32 {
        self.clock.saturating_sub(u32::from(self.age))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BackfillRecord {
    pub clock: u32,
    pub glucose: u16,
    pub state: u8,
    pub trend: Option<f32>,
}

impl BackfillRecord {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let state = data[6];
        let trend = data[8] as i8;
        let clock = u32_at(data, 0);
        let glucose = trusted_glucose(u16_at(data, 4), state)?;
        (clock != 0).then_some(Self {
            clock,
            glucose,
            state,
            trend: (trend != TREND_UNKNOWN).then_some(f32::from(trend) / 10.0),
        })
    }
}
