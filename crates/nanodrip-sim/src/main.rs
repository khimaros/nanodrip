//! host binary exposing nanodrip over stdin/stdout, which is the seam the
//! python end-to-end tests drive. each command is answered with zero or more
//! event lines followed by "ok".

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use nanodrip_app::{serve, Feed, MqttConfig, Settings, HISTORY_SLOTS};
use nanodrip_core::{Action, Channel, Session};
use rand_core::OsRng;

const DEFAULT_DEVICE: &str = "sim";
const KEY_SIZE: usize = 16;

fn to_hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).expect("hex input"))
        .collect()
}

fn parse_args() -> HashMap<String, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (pairs, _) = args.as_chunks::<2>();
    pairs
        .iter()
        .map(|[name, value]| (name.trim_start_matches('-').to_string(), value.clone()))
        .collect()
}

fn load_key(path: &str) -> Option<[u8; KEY_SIZE]> {
    let text = fs::read_to_string(path).ok()?;
    from_hex(text.trim()).try_into().ok()
}

fn initial_settings(args: &HashMap<String, String>) -> Settings {
    let part = |name: &str| args.get(name).map(|hex| from_hex(hex)).unwrap_or_default();
    Settings {
        code: args.get("code").cloned(),
        part_a: part("part-a"),
        part_b: part("part-b"),
        part_c: part("part-c"),
        mqtt: args.get("mqtt").map(|address| MqttConfig {
            address: address.clone(),
            user: args.get("mqtt-user").cloned(),
            password: args.get("mqtt-pass").cloned(),
        }),
        ..Settings::default()
    }
}

fn build_feed(args: &HashMap<String, String>) -> Feed {
    let device = args.get("device").map_or(DEFAULT_DEVICE, String::as_str);
    let feed = Feed::new(device);
    *feed.shared().settings.lock().unwrap() = initial_settings(args);
    feed
}

fn channel_name(channel: Channel) -> &'static str {
    match channel {
        Channel::Auth => "auth",
        Channel::Extra => "extra",
        Channel::Control => "control",
        Channel::Backfill => "backfill",
    }
}

fn channel_from(name: &str) -> Channel {
    match name {
        "auth" => Channel::Auth,
        "extra" => Channel::Extra,
        "control" => Channel::Control,
        "backfill" => Channel::Backfill,
        other => panic!("unknown channel {other}"),
    }
}

struct Host {
    feed: Feed,
    session: Option<Session>,
    generation: u32,
    key_file: String,
    history_dir: Option<PathBuf>,
    last_seen: Option<u32>,
    now: Option<u64>,
}

fn slot_path(dir: &Path, slot: u16) -> PathBuf {
    dir.join(format!("slot-{slot}.bin"))
}

impl Host {
    fn restore_history(&self) {
        let Some(dir) = &self.history_dir else { return };
        for slot in 0..HISTORY_SLOTS {
            if let Ok(data) = fs::read(slot_path(dir, slot)) {
                self.feed.restore_slot(&data);
            }
        }
    }

    fn save_history(&mut self) {
        let changes = self.feed.take_history_changes();
        let Some(dir) = &self.history_dir else { return };
        fs::create_dir_all(dir).expect("create history dir");
        if changes.clear {
            for slot in 0..HISTORY_SLOTS {
                let _ = fs::remove_file(slot_path(dir, slot));
            }
        }
        for (slot, data) in changes.slots {
            fs::write(slot_path(dir, slot), data).expect("write history");
        }
    }

    fn now(&self) -> u64 {
        self.now.unwrap_or_else(|| {
            let elapsed = SystemTime::now().duration_since(UNIX_EPOCH);
            elapsed.expect("clock after epoch").as_secs()
        })
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::Write(channel, data) => {
                println!("tx {} {}", channel_name(channel), to_hex(&data))
            }
            Action::RequestBond => println!("bond"),
            Action::SaveKey(key) => {
                fs::write(&self.key_file, to_hex(&key)).expect("write key file")
            }
            Action::ClearKey => self.clear_key(),
            Action::Reading(reading) => {
                let now = self.now();
                let entry = self.feed.record_reading(&reading, now);
                println!("reading {}", entry.to_json());
            }
            Action::Backfill(record) => {
                println!("backfill {}", self.feed.record_backfill(&record).to_json())
            }
            Action::Fail(error) => self.report(error.name()),
            Action::Disconnect => println!("disconnect"),
        }
    }

    fn clear_key(&self) {
        let _ = fs::remove_file(&self.key_file);
    }

    fn report(&self, error: &'static str) {
        self.feed.set_error(Some(error));
        println!("fail {error}");
    }

    /// starts a connection from the current settings, first dropping whatever
    /// belonged to the previous sensor if the pairing changed.
    fn connect(&mut self) -> Vec<Action> {
        let settings = self.feed.shared().settings.lock().unwrap().clone();
        if settings.generation != self.generation {
            self.generation = settings.generation;
            self.clear_key();
            self.feed.reset_sensor();
            self.last_seen = None;
        }
        self.feed.set_error(None);
        let Some(mut config) = settings.config() else {
            self.session = None;
            self.report("unpaired");
            return Vec::new();
        };
        config.saved_key = load_key(&self.key_file);
        config.last_seen = self.last_seen.or(self.feed.last_seen());
        let session = self.session.insert(Session::new(config));
        session.on_connected(&mut OsRng)
    }

    fn dispatch(&mut self, line: &str) -> Vec<Action> {
        let mut words = line.split_whitespace();
        let command = (words.next(), words.next(), words.next());
        if command.0 == Some("connected") {
            return self.connect();
        }
        let Some(session) = self.session.as_mut() else {
            return Vec::new();
        };
        match command {
            (Some("bonded"), ..) => session.on_bonded(),
            (Some("idle"), ..) => session.on_idle(),
            (Some("rx"), Some(channel), Some(data)) => {
                session.on_rx(&mut OsRng, channel_from(channel), &from_hex(data))
            }
            _ => Vec::new(),
        }
    }
}

fn start_web(args: &HashMap<String, String>, host: &Host) {
    let Some(address) = args.get("http") else {
        return;
    };
    let listener = TcpListener::bind(address).expect("bind http");
    println!("http {}", listener.local_addr().expect("http addr").port());
    let shared = host.feed.shared();
    thread::spawn(move || serve(listener, shared));
}

fn main() {
    let args = parse_args();
    let mut host = Host {
        feed: build_feed(&args),
        session: None,
        generation: 0,
        key_file: args["key-file"].clone(),
        history_dir: args.get("history-dir").map(PathBuf::from),
        last_seen: args.get("last-seen").map(|v| v.parse().expect("last-seen")),
        now: args.get("now").map(|v| v.parse().expect("now")),
    };
    host.restore_history();
    start_web(&args, &host);
    for line in io::stdin().lock().lines() {
        for action in host.dispatch(&line.expect("stdin")) {
            host.apply(action);
        }
        host.save_history();
        host.feed.flush();
        println!("ok");
        io::stdout().flush().expect("stdout");
    }
}
