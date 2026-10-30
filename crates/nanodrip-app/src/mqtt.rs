//! minimal mqtt 3.1.1 publisher. each flush is one short connection, which
//! suits a feed that only has something to say every few minutes.

use std::io::{self, Error, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);
const PROTOCOL_LEVEL: u8 = 4;
const KEEP_ALIVE_SECONDS: u16 = 60;
const FLAG_CLEAN_SESSION: u8 = 0x02;
const FLAG_PASSWORD: u8 = 0x40;
const FLAG_USERNAME: u8 = 0x80;
const PACKET_CONNECT: u8 = 0x10;
const PACKET_CONNACK: u8 = 0x20;
const PACKET_PUBLISH_QOS1: u8 = 0x32;
const PACKET_PUBACK: u8 = 0x40;
const PACKET_DISCONNECT: u8 = 0xe0;
const FLAG_RETAIN: u8 = 0x01;

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub topic: String,
    pub payload: String,
    pub retain: bool,
}

fn write_string(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
}

fn frame(header: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![header];
    let mut size = body.len();
    loop {
        let byte = (size % 128) as u8;
        size /= 128;
        out.push(if size > 0 { byte | 0x80 } else { byte });
        if size == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
    out
}

fn connect_packet(client_id: &str, user: Option<&str>, password: Option<&str>) -> Vec<u8> {
    let mut flags = FLAG_CLEAN_SESSION;
    flags |= user.map_or(0, |_| FLAG_USERNAME);
    flags |= password.map_or(0, |_| FLAG_PASSWORD);
    let mut body = Vec::new();
    write_string(&mut body, b"MQTT");
    body.extend_from_slice(&[PROTOCOL_LEVEL, flags]);
    body.extend_from_slice(&KEEP_ALIVE_SECONDS.to_be_bytes());
    write_string(&mut body, client_id.as_bytes());
    for field in [user, password].into_iter().flatten() {
        write_string(&mut body, field.as_bytes());
    }
    frame(PACKET_CONNECT, &body)
}

fn publish_packet(message: &Message, packet_id: u16) -> Vec<u8> {
    let mut body = Vec::new();
    write_string(&mut body, message.topic.as_bytes());
    body.extend_from_slice(&packet_id.to_be_bytes());
    body.extend_from_slice(message.payload.as_bytes());
    let retain = if message.retain { FLAG_RETAIN } else { 0 };
    frame(PACKET_PUBLISH_QOS1 | retain, &body)
}

fn read_ack(stream: &mut TcpStream, expected: u8) -> io::Result<[u8; 2]> {
    let mut packet = [0u8; 4];
    stream.read_exact(&mut packet)?;
    if packet[0] != expected {
        return Err(Error::new(ErrorKind::InvalidData, "unexpected mqtt packet"));
    }
    Ok([packet[2], packet[3]])
}

pub struct Connection {
    stream: TcpStream,
    packet_id: u16,
}

impl Connection {
    pub fn open(
        address: &str,
        client_id: &str,
        user: Option<&str>,
        password: Option<&str>,
    ) -> io::Result<Self> {
        let target = address
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "unresolved mqtt host"))?;
        let mut stream = TcpStream::connect_timeout(&target, TIMEOUT)?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        stream.write_all(&connect_packet(client_id, user, password))?;
        let [_, code] = read_ack(&mut stream, PACKET_CONNACK)?;
        if code != 0 {
            return Err(Error::new(ErrorKind::PermissionDenied, "mqtt refused"));
        }
        Ok(Self {
            stream,
            packet_id: 0,
        })
    }

    /// publishes at qos 1, returning once the broker holds the message.
    pub fn publish(&mut self, message: &Message) -> io::Result<()> {
        self.packet_id = self.packet_id.wrapping_add(1).max(1);
        self.stream
            .write_all(&publish_packet(message, self.packet_id))?;
        let id = read_ack(&mut self.stream, PACKET_PUBACK)?;
        if u16::from_be_bytes(id) != self.packet_id {
            return Err(Error::new(ErrorKind::InvalidData, "mqtt ack mismatch"));
        }
        Ok(())
    }

    pub fn close(mut self) {
        let _ = self.stream.write_all(&[PACKET_DISCONNECT, 0]);
    }
}
