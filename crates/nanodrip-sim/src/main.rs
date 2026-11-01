//! host binary exposing nanodrip over stdin/stdout, which is the seam the
//! python end-to-end tests drive. each command is answered with zero or more
//! event lines followed by "ok".

use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{self, BufRead, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use nanodrip_app::{
    serve, Feed, Flash, Log, MqttConfig, SensorId, SensorName, Settings, SECTOR_SIZE,
};
use nanodrip_core::{Action, Channel, Session};
use rand_core::OsRng;

const DEFAULT_DEVICE: &str = "sim";
const DEFAULT_LOG_SECTORS: u32 = 64;
const DEFAULT_TAG_SECTORS: u32 = 4;
const KEY_SIZE: usize = 16;
const ERASED: u8 = 0xFF;

/// flash emulated by a file, padded out so a shorter image can be used as one.
struct FileFlash {
    file: File,
    sectors: u32,
}

impl FileFlash {
    fn open(path: &Path, sectors: u32) -> io::Result<Self> {
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let size = sectors as usize * SECTOR_SIZE;
        let held = file.seek(SeekFrom::End(0))? as usize;
        file.write_all(&vec![ERASED; size.saturating_sub(held)])?;
        Ok(Self { file, sectors })
    }
}

impl Flash for FileFlash {
    fn sectors(&self) -> u32 {
        self.sectors
    }

    fn read(&self, at: u32, data: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(data, u64::from(at))
    }

    fn write(&mut self, at: u32, data: &[u8]) -> io::Result<()> {
        self.file.write_all_at(data, u64::from(at))
    }

    fn erase(&mut self, sector: u32) -> io::Result<()> {
        let at = u64::from(sector) * SECTOR_SIZE as u64;
        self.file.write_all_at(&[ERASED; SECTOR_SIZE], at)
    }
}

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
    if let Some(capacity) = args.get("history-capacity") {
        let capacity = capacity.parse().expect("history-capacity");
        feed.shared().store.lock().unwrap().set_capacity(capacity);
    }
    feed
}

/// opens the log in `--history-dir` and loads what it holds into the feed.
fn open_log(args: &HashMap<String, String>, feed: &Feed) -> Option<Log<FileFlash>> {
    let dir = Path::new(args.get("history-dir")?);
    fs::create_dir_all(dir).expect("create history dir");
    let sectors = args
        .get("log-sectors")
        .map_or(DEFAULT_LOG_SECTORS, |v| v.parse().expect("log-sectors"));
    let flash = FileFlash::open(&dir.join("log.bin"), sectors).expect("open log");
    let mut log = Log::new(flash);
    let shared = feed.shared();
    log.load(&mut shared.store.lock().unwrap())
        .expect("load log");
    let sectors = args
        .get("tag-sectors")
        .map_or(DEFAULT_TAG_SECTORS, |v| v.parse().expect("tag-sectors"));
    let flash = FileFlash::open(&dir.join("tags.bin"), sectors).expect("open tags");
    let mut tags = shared.tags.lock().unwrap();
    tags.attach(Box::new(flash)).expect("load tags");
    drop(tags);
    Some(log)
}

fn parse_id(text: Option<&str>) -> SensorId {
    text.map_or([0; 6], |hex| from_hex(hex).try_into().expect("sensor id"))
}

fn parse_name(text: Option<&str>) -> SensorName {
    let mut name = [0; 8];
    let text = text.unwrap_or("").as_bytes();
    name[..text.len()].copy_from_slice(text);
    name
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
    log: Option<Log<FileFlash>>,
    last_seen: Option<u32>,
    now: Option<u64>,
}

impl Host {
    fn save_history(&mut self) {
        let changes = self.feed.take_unsaved();
        if let Some(log) = self.log.as_mut() {
            log.save(changes).expect("write log");
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
            Action::HistoryExhausted => self.feed.history_exhausted(),
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

    /// starts a connection to a sensor from the current settings. a changed
    /// pairing only needs a new key, as history is kept per sensor.
    fn connect(&mut self, id: SensorId, name: SensorName) -> Vec<Action> {
        let settings = self.feed.shared().settings.lock().unwrap().clone();
        if settings.generation != self.generation {
            self.generation = settings.generation;
            self.clear_key();
            self.last_seen = None;
        }
        self.feed.select_sensor(id, name);
        self.feed.set_error(None);
        let Some(mut config) = settings.config() else {
            self.session = None;
            self.report("unpaired");
            return Vec::new();
        };
        config.saved_key = load_key(&self.key_file);
        config.last_seen = self.last_seen.or(self.feed.last_seen());
        config.oldest = self.feed.oldest();
        let session = self.session.insert(Session::new(config));
        session.on_connected(&mut OsRng)
    }

    fn dispatch(&mut self, line: &str) -> Vec<Action> {
        let mut words = line.split_whitespace();
        let command = (words.next(), words.next(), words.next());
        if command.0 == Some("connected") {
            return self.connect(parse_id(command.1), parse_name(command.2));
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
    let feed = build_feed(&args);
    let log = open_log(&args, &feed);
    let mut host = Host {
        feed,
        session: None,
        generation: 0,
        key_file: args["key-file"].clone(),
        log,
        last_seen: args.get("last-seen").map(|v| v.parse().expect("last-seen")),
        now: args.get("now").map(|v| v.parse().expect("now")),
    };
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
