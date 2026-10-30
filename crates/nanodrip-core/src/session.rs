//! one ble connection to a sensor, as a state machine over notifications.
//! the message order follows xdrip libkeks, which is the working reference
//! for this undocumented protocol.

use alloc::vec::Vec;

use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey;
use rand_core::CryptoRngCore;

use crate::jpake::{Jpake, SharedKey, KEY_SIZE, PACKET_SIZE};
use crate::message::*;

const CHUNK_SIZE: usize = 20;
const SHORT_CODE_LEN: usize = 4;
const LONG_CODE_LEN: usize = 6;
const LONG_CODE_PREFIX: &[u8] = b"00";
const MIN_CERT_LEN: usize = 100;
const SIGN_RX_OFFSET: usize = 2;
/// one missed five minute reading plus slack before history is requested.
pub const BACKFILL_GAP_SECONDS: u32 = 420;
/// the history window xdrip requests from these sensors.
pub const BACKFILL_WINDOW_SECONDS: u32 = 86_400;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Channel {
    Auth,
    Extra,
    Control,
    Backfill,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Error {
    Proof,
    Challenge,
    Rejected,
    MissingCertificates,
    Certificate,
    Protocol,
    Timeout,
}

impl Error {
    pub fn name(&self) -> &'static str {
        match self {
            Error::Proof => "proof",
            Error::Challenge => "challenge",
            Error::Rejected => "rejected",
            Error::MissingCertificates => "missing-certificates",
            Error::Certificate => "certificate",
            Error::Protocol => "protocol",
            Error::Timeout => "timeout",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Write(Channel, Vec<u8>),
    RequestBond,
    SaveKey([u8; KEY_SIZE]),
    ClearKey,
    Reading(Reading),
    Backfill(BackfillRecord),
    Fail(Error),
    Disconnect,
}

#[derive(Default)]
pub struct Config {
    code: Vec<u8>,
    pub part_a: Vec<u8>,
    pub part_b: Vec<u8>,
    pub part_c: Vec<u8>,
    pub saved_key: Option<SharedKey>,
    pub last_seen: Option<u32>,
}

impl Config {
    /// accepts the four digit stelo/g7 code or a six character one+ style code.
    pub fn new(code: &str) -> Option<Self> {
        if !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let code = match code.len() {
            SHORT_CODE_LEN => code.as_bytes().to_vec(),
            LONG_CODE_LEN => [LONG_CODE_PREFIX, code.as_bytes()].concat(),
            _ => return None,
        };
        Some(Self {
            code,
            ..Self::default()
        })
    }

    fn short_code(&self) -> bool {
        self.code.len() == SHORT_CODE_LEN
    }

    fn has_certificates(&self) -> bool {
        [&self.part_a, &self.part_b, &self.part_c]
            .iter()
            .all(|part| part.len() > MIN_CERT_LEN)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Idle,
    Round(usize),
    RequestAuth,
    ChallengeReply,
    CertInfo(u8),
    CertOut(u8),
    SignChallenge,
    SignOut,
    AwaitBondEcho,
    Bonding,
    Data,
    Backfill,
    Done,
    Failed,
}

pub struct Session {
    config: Config,
    state: State,
    jpake: Option<Jpake>,
    fresh_key: Option<SharedKey>,
    token: [u8; TOKEN_SIZE],
    buffer: Vec<u8>,
    expected: usize,
}

impl Session {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            state: State::Idle,
            jpake: None,
            fresh_key: None,
            token: [0; TOKEN_SIZE],
            buffer: Vec::new(),
            expected: 0,
        }
    }

    pub fn on_connected(&mut self, rng: &mut impl CryptoRngCore) -> Vec<Action> {
        self.buffer.clear();
        if self.config.saved_key.is_some() {
            self.state = State::RequestAuth;
            return alloc::vec![self.auth_request(rng)];
        }
        self.jpake = Some(Jpake::new(rng, &self.config.code));
        self.state = State::Round(0);
        alloc::vec![auth(alloc::vec![OP_ROUND, 0])]
    }

    pub fn on_rx(
        &mut self,
        rng: &mut impl CryptoRngCore,
        channel: Channel,
        data: &[u8],
    ) -> Vec<Action> {
        match channel {
            Channel::Auth => self.on_auth(data),
            Channel::Extra => self.on_extra(rng, data),
            Channel::Control => self.on_control(data),
            Channel::Backfill => self.on_backfill(data),
        }
    }

    /// the ble link finished pairing, so the control channel is now readable.
    pub fn on_bonded(&mut self) -> Vec<Action> {
        if self.state != State::Bonding {
            return Vec::new();
        }
        self.start_data()
    }

    /// the host saw no traffic for a while: end a finished session or give up.
    pub fn on_idle(&mut self) -> Vec<Action> {
        match self.state {
            State::Data | State::Backfill => {
                self.state = State::Done;
                alloc::vec![
                    Action::Write(Channel::Control, alloc::vec![OP_DISCONNECT]),
                    Action::Disconnect,
                ]
            }
            State::Idle | State::Done | State::Failed => Vec::new(),
            _ => self.fail(Error::Timeout),
        }
    }

    fn fail(&mut self, error: Error) -> Vec<Action> {
        self.state = State::Failed;
        alloc::vec![Action::Fail(error), Action::Disconnect]
    }

    fn auth_request(&mut self, rng: &mut impl CryptoRngCore) -> Action {
        rng.fill_bytes(&mut self.token);
        let mut message = alloc::vec![OP_AUTH_REQUEST];
        message.extend_from_slice(&self.token);
        message.push(AUTH_SLOT);
        auth(message)
    }

    fn key(&self) -> Option<SharedKey> {
        self.fresh_key.or(self.config.saved_key)
    }

    fn on_extra(&mut self, rng: &mut impl CryptoRngCore, data: &[u8]) -> Vec<Action> {
        match self.state {
            State::Round(round) => {
                self.buffer.extend_from_slice(data);
                if self.buffer.len() < PACKET_SIZE {
                    return Vec::new();
                }
                self.on_round(rng, round)
            }
            // the sensor starts its certificate before it answers our request
            State::CertInfo(_) => {
                self.buffer.extend_from_slice(data);
                Vec::new()
            }
            State::CertOut(which) => {
                self.buffer.extend_from_slice(data);
                if self.buffer.len() < self.expected {
                    return Vec::new();
                }
                self.after_certificate(rng, which)
            }
            _ => Vec::new(),
        }
    }

    fn on_round(&mut self, rng: &mut impl CryptoRngCore, round: usize) -> Vec<Action> {
        let packet = core::mem::take(&mut self.buffer);
        let Some(jpake) = self.jpake.as_mut() else {
            return self.fail(Error::Protocol);
        };
        if !jpake.accept(round, &packet) {
            return self.fail(Error::Proof);
        }
        let ours = jpake.round(rng, round);
        let mut actions = chunks(&ours);
        if round + 1 < 3 {
            self.state = State::Round(round + 1);
            actions.push(auth(alloc::vec![OP_ROUND, round as u8 + 1]));
        } else {
            self.fresh_key = Some(jpake.shared_key());
            self.state = State::RequestAuth;
            actions.push(self.auth_request(rng));
        }
        actions
    }

    fn on_auth(&mut self, data: &[u8]) -> Vec<Action> {
        match self.state {
            State::RequestAuth => self.on_challenge(data),
            State::ChallengeReply => self.on_status(data),
            State::CertInfo(which) => self.on_cert_info(which, data),
            State::SignChallenge => self.on_sign_challenge(data),
            State::SignOut => {
                self.state = State::AwaitBondEcho;
                alloc::vec![auth(KEEP_ALIVE.to_vec())]
            }
            State::AwaitBondEcho if BOND_ECHOES.contains(&data) => {
                self.state = State::Bonding;
                alloc::vec![Action::RequestBond]
            }
            _ => Vec::new(),
        }
    }

    fn on_challenge(&mut self, data: &[u8]) -> Vec<Action> {
        let (Some((proof, challenge)), Some(key)) = (parse_challenge(data), self.key()) else {
            return self.fail(Error::Protocol);
        };
        // xdrip only checks the proof on a fresh exchange
        if self.fresh_key.is_some() && proof != auth_hash(&key, &self.token) {
            return self.fail(Error::Challenge);
        }
        self.state = State::ChallengeReply;
        let mut message = alloc::vec![OP_AUTH_CHALLENGE_TX];
        message.extend_from_slice(&auth_hash(&key, &challenge));
        alloc::vec![auth(message)]
    }

    fn on_status(&mut self, data: &[u8]) -> Vec<Action> {
        let Some(status) = parse_status(data) else {
            return self.fail(Error::Protocol);
        };
        if !status.authenticated {
            let mut actions = alloc::vec![Action::ClearKey];
            actions.extend(self.fail(Error::Rejected));
            return actions;
        }
        let mut actions = Vec::new();
        if status.refresh {
            actions.push(Action::ClearKey);
        } else if let Some(key) = self.fresh_key {
            actions.push(Action::SaveKey(key));
        }
        actions.extend(self.after_auth(status.bonded));
        actions
    }

    fn after_auth(&mut self, bonded: bool) -> Vec<Action> {
        if bonded {
            return self.start_data();
        }
        if !self.config.short_code() {
            self.state = State::AwaitBondEcho;
            return alloc::vec![auth(KEEP_ALIVE.to_vec())];
        }
        if !self.config.has_certificates() {
            return self.fail(Error::MissingCertificates);
        }
        self.cert_request(0)
    }

    fn cert_request(&mut self, which: u8) -> Vec<Action> {
        self.state = State::CertInfo(which);
        let part = if which == 0 {
            &self.config.part_a
        } else {
            &self.config.part_b
        };
        let mut message = alloc::vec![OP_CERT_INFO, which];
        message.extend_from_slice(&(part.len() as u32).to_le_bytes());
        alloc::vec![auth(message)]
    }

    fn on_cert_info(&mut self, which: u8, data: &[u8]) -> Vec<Action> {
        let Some(size) = parse_cert_info(data) else {
            return self.fail(Error::Certificate);
        };
        self.expected = size;
        self.state = State::CertOut(which);
        let part = if which == 0 {
            &self.config.part_a
        } else {
            &self.config.part_b
        };
        chunks(part)
    }

    fn after_certificate(&mut self, rng: &mut impl CryptoRngCore, which: u8) -> Vec<Action> {
        self.buffer.clear();
        if which == 0 {
            return self.cert_request(1);
        }
        self.state = State::SignChallenge;
        let mut token = [0u8; SIGN_TOKEN_SIZE];
        rng.fill_bytes(&mut token);
        let mut message = alloc::vec![OP_SIGN_CHALLENGE];
        message.extend_from_slice(&token);
        alloc::vec![auth(message)]
    }

    fn on_sign_challenge(&mut self, data: &[u8]) -> Vec<Action> {
        let end = SIGN_RX_OFFSET + SIGN_TOKEN_SIZE;
        if data.len() < end || data[1] != 0 {
            return self.fail(Error::Certificate);
        }
        let Ok(key) = SigningKey::from_pkcs8_der(&self.config.part_c) else {
            return self.fail(Error::Certificate);
        };
        let signature: Signature = key.sign(&data[SIGN_RX_OFFSET..end]);
        self.state = State::SignOut;
        let mut actions = chunks(&signature.to_bytes());
        actions.push(auth(CHALLENGE_OUT.to_vec()));
        actions
    }

    fn start_data(&mut self) -> Vec<Action> {
        self.state = State::Data;
        let request = if self.config.short_code() {
            alloc::vec![OP_GLUCOSE]
        } else {
            LONG_GLUCOSE_REQUEST.to_vec()
        };
        alloc::vec![Action::Write(Channel::Control, request)]
    }

    fn on_control(&mut self, data: &[u8]) -> Vec<Action> {
        if self.state != State::Data {
            return Vec::new();
        }
        let Some(reading) = Reading::parse(data) else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        if let Some(start) = self.backfill_start(&reading) {
            let mut message = alloc::vec![OP_BACKFILL];
            message.extend_from_slice(&start.to_le_bytes());
            message.extend_from_slice(&reading.clock.to_le_bytes());
            self.state = State::Backfill;
            self.buffer.clear();
            actions.push(Action::Write(Channel::Control, message));
        }
        actions.insert(0, Action::Reading(reading));
        actions
    }

    /// where history should resume from, if anything is missing. with nothing
    /// stored yet, that is as far back as the sensor keeps.
    fn backfill_start(&self, reading: &Reading) -> Option<u32> {
        match self.config.last_seen {
            Some(last) => (reading.measured_at().saturating_sub(last) > BACKFILL_GAP_SECONDS)
                .then_some(last + 1),
            None => Some(reading.clock.saturating_sub(BACKFILL_WINDOW_SECONDS).max(1)),
        }
    }

    fn on_backfill(&mut self, data: &[u8]) -> Vec<Action> {
        if self.state != State::Backfill {
            return Vec::new();
        }
        self.buffer.extend_from_slice(data);
        let whole = self.buffer.len() - self.buffer.len() % BACKFILL_RECORD_SIZE;
        let (records, _) = self.buffer[..whole].as_chunks::<BACKFILL_RECORD_SIZE>();
        let actions = records
            .iter()
            .filter_map(|record| BackfillRecord::parse(record))
            .map(Action::Backfill)
            .collect();
        self.buffer.drain(..whole);
        actions
    }
}

fn auth(message: Vec<u8>) -> Action {
    Action::Write(Channel::Auth, message)
}

fn chunks(data: &[u8]) -> Vec<Action> {
    data.chunks(CHUNK_SIZE)
        .map(|chunk| Action::Write(Channel::Extra, chunk.to_vec()))
        .collect()
}
