//! usbmux spoken directly over USB, for devices the system usbmuxd ignores.
//!
//! Current macOS no longer recognises iPhone OS 1.x devices such as the first
//! iPod touch: they stay in USB configuration 1 (PTP only). This module
//! switches them to the configuration with the usbmux interface (class 255,
//! subclass 254, protocol 2) and speaks the protocol itself, as the
//! open-source usbmuxd does on Linux.
//!
//! Every packet starts with a big-endian mux header: protocol and total
//! length, followed in protocol version 2 by a magic number and two sequence
//! counters. Protocol 0 exchanges versions; protocol 6 carries a simplified
//! TCP: a plain 20-byte TCP header, no checksums, options or retransmission,
//! and windows counted in 256-byte units. Connections are closed with RST.

use crate::{Error, Result};
use nusb::transfer::{Bulk, In, Out};
use nusb::Endpoint;
use nusb::{DeviceInfo, MaybeFuture};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::ops::RangeInclusive;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

const APPLE: u16 = 0x05ac;
/// USB product IDs of iPhones, iPod touches and iPads.
const IOS_PRODUCTS: RangeInclusive<u16> = 0x1290..=0x12af;

const PROTO_VERSION: u32 = 0;
const PROTO_SETUP: u32 = 2;
const PROTO_TCP: u32 = 6;
const MAGIC: u32 = 0xfeed_face;

const TH_SYN: u8 = 0x02;
const TH_RST: u8 = 0x04;
const TH_ACK: u8 = 0x10;
const TCP_HEADER: usize = 20;

/// Size of each USB read; longer packets span several reads.
const USB_MRU: usize = 16384;
/// Largest packet sent at once, as in usbmuxd.
const USB_MTU: usize = 3 * 16384;
/// Receive window we advertise. Incoming data is buffered without limit and
/// acknowledged at once, so this only paces the device.
const RX_WINDOW: u32 = 128 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

/// iOS devices on the USB bus, whether or not anything is talking to them.
pub fn list() -> Result<Vec<DeviceInfo>> {
    Ok(nusb::list_devices()
        .wait()?
        .filter(|d| d.vendor_id() == APPLE && IOS_PRODUCTS.contains(&d.product_id()))
        .collect())
}

/// An open usbmux session with one device.
pub struct Mux {
    shared: Arc<Shared>,
}

struct Shared {
    writer: Mutex<Writer>,
    conns: Mutex<Conns>,
}

struct Conns {
    by_port: HashMap<u16, Arc<Conn>>,
    next_port: u16,
    /// Set when the device goes away.
    dead: bool,
}

struct Writer {
    ep: Endpoint<Bulk, Out>,
    version: u32,
    tx_seq: u16,
    rx_seq: u16,
}

impl Mux {
    /// Switches the device to its usbmux configuration if needed, claims the
    /// interface and exchanges protocol versions.
    pub fn open(info: &DeviceInfo) -> Result<Mux> {
        let device = info.open().wait()?;
        let (config, interface, ep_in, ep_out) = device
            .configurations()
            .find_map(|c| {
                let alt = c
                    .interface_alt_settings()
                    .find(|i| (i.class(), i.subclass(), i.protocol()) == (255, 254, 2))?;
                let ep = |dir: u8| {
                    alt.endpoints()
                        .map(|e| e.address())
                        .find(|a| a & 0x80 == dir)
                };
                Some((
                    c.configuration_value(),
                    alt.interface_number(),
                    ep(0x80)?,
                    ep(0)?,
                ))
            })
            .ok_or_else(|| Error::protocol("device has no usbmux interface"))?;
        let active = device
            .active_configuration()
            .map(|c| c.configuration_value());
        if active.ok() != Some(config) {
            device.set_configuration(config).wait()?;
        }
        let interface = device.claim_interface(interface).wait()?;
        let mut reader = Reader {
            ep: interface.endpoint::<Bulk, In>(ep_in)?,
            buf: Vec::new(),
        };
        let mut writer = Writer {
            ep: interface.endpoint::<Bulk, Out>(ep_out)?,
            version: 0,
            tx_seq: 0,
            rx_seq: 0,
        };

        writer.send(PROTO_VERSION, &version_header(2, 0), &[])?;
        let reply = loop {
            let packet = reader.next_packet(Some(TIMEOUT))?;
            if be32(&packet, 0) == PROTO_VERSION {
                break packet;
            }
        };
        let major = reply
            .get(8..12)
            .map(|_| be32(&reply, 8))
            .ok_or_else(|| Error::protocol("short version reply"))?;
        if !(1..=2).contains(&major) {
            return Err(Error::protocol(format!("unknown usbmux version {major}")));
        }
        writer.version = major;
        if major >= 2 {
            writer.send(PROTO_SETUP, &[], &[0x07])?;
        }

        let shared = Arc::new(Shared {
            writer: Mutex::new(writer),
            conns: Mutex::new(Conns {
                by_port: HashMap::new(),
                next_port: 1,
                dead: false,
            }),
        });
        let weak = Arc::downgrade(&shared);
        std::thread::Builder::new()
            .name("usbmux-reader".into())
            .spawn(move || read_loop(reader, weak))?;
        Ok(Mux { shared })
    }

    /// Opens a TCP connection to `port` on the device.
    pub fn connect(&self, port: u16) -> Result<Connection> {
        let conn = Arc::new(Conn {
            state: Mutex::new(ConnState::default()),
            changed: Condvar::new(),
        });
        let sport = {
            let mut conns = lock(&self.shared.conns);
            if conns.dead {
                return Err(Error::Refused("device disconnected".into()));
            }
            let mut port = conns.next_port;
            while conns.by_port.contains_key(&port) || port == 0 {
                port = port.wrapping_add(1);
            }
            conns.next_port = port.wrapping_add(1);
            conns.by_port.insert(port, conn.clone());
            port
        };
        let connection = Connection {
            shared: self.shared.clone(),
            conn,
            sport,
            dport: port,
        };
        {
            let state = lock(&connection.conn.state);
            connection.send_tcp(&state, TH_SYN, &[])?;
            let (state, _) = connection
                .conn
                .changed
                .wait_timeout_while(state, TIMEOUT, |s| s.phase == Phase::Connecting)
                .unwrap_or_else(|e| e.into_inner());
            match state.phase {
                Phase::Open => {}
                Phase::Connecting => return Err(Error::protocol("connect timed out")),
                Phase::Closed => {
                    return Err(Error::Refused(format!(
                        "nothing listening on device port {port}"
                    )))
                }
            }
        }
        Ok(connection)
    }
}

/// A TCP connection to a device port, tunnelled over usbmux.
pub struct Connection {
    shared: Arc<Shared>,
    conn: Arc<Conn>,
    sport: u16,
    dport: u16,
}

struct Conn {
    state: Mutex<ConnState>,
    changed: Condvar,
}

#[derive(Default)]
struct ConnState {
    phase: Phase,
    received: VecDeque<u8>,
    /// Our next sequence number and the next one we expect from the device.
    tx_seq: u32,
    tx_ack: u32,
    /// What the device has acknowledged, and its window.
    peer_ack: u32,
    peer_window: u32,
}

#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum Phase {
    #[default]
    Connecting,
    Open,
    Closed,
}

impl Connection {
    /// Sends a TCP packet. Called with the state locked, so that sequence
    /// numbers go out in order.
    fn send_tcp(&self, state: &ConnState, flags: u8, payload: &[u8]) -> Result<()> {
        let header = tcp_header(self.sport, self.dport, state.tx_seq, state.tx_ack, flags);
        lock(&self.shared.writer).send(PROTO_TCP, &header, payload)
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let state = lock(&self.conn.state);
        let mut state = self
            .conn
            .changed
            .wait_while(state, |s| s.received.is_empty() && s.phase == Phase::Open)
            .unwrap_or_else(|e| e.into_inner());
        let n = buf.len().min(state.received.len());
        for (dst, src) in buf.iter_mut().zip(state.received.drain(..n)) {
            *dst = src;
        }
        Ok(n)
    }
}

impl Write for Connection {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let state = lock(&self.conn.state);
        let in_flight = |s: &ConnState| s.tx_seq.wrapping_sub(s.peer_ack);
        let (mut state, _) = self
            .conn
            .changed
            .wait_timeout_while(state, TIMEOUT, |s| {
                s.phase == Phase::Open && in_flight(s) >= s.peer_window
            })
            .unwrap_or_else(|e| e.into_inner());
        if state.phase != Phase::Open {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let room = state.peer_window.saturating_sub(in_flight(&state)) as usize;
        if room == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "device stopped reading",
            ));
        }
        let n = data.len().min(room).min(mss(&self.shared));
        self.send_tcp(&state, TH_ACK, &data[..n])
            .map_err(io::Error::other)?;
        state.tx_seq = state.tx_seq.wrapping_add(n as u32);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let mut state = lock(&self.conn.state);
        if state.phase == Phase::Open {
            let _ = self.send_tcp(&state, TH_RST, &[]);
            state.phase = Phase::Closed;
        }
        drop(state);
        lock(&self.shared.conns).by_port.remove(&self.sport);
    }
}

/// Largest TCP payload per packet.
fn mss(shared: &Shared) -> usize {
    USB_MTU - mux_header_len(lock(&shared.writer).version) - TCP_HEADER
}

fn mux_header_len(version: u32) -> usize {
    if version < 2 {
        8
    } else {
        16
    }
}

impl Writer {
    fn send(&mut self, protocol: u32, header: &[u8], payload: &[u8]) -> Result<()> {
        let total = mux_header_len(self.version) + header.len() + payload.len();
        let mut packet = Vec::with_capacity(total);
        packet.extend(protocol.to_be_bytes());
        packet.extend((total as u32).to_be_bytes());
        if self.version >= 2 {
            if protocol == PROTO_SETUP {
                self.tx_seq = 0;
                self.rx_seq = 0xffff;
            }
            packet.extend(MAGIC.to_be_bytes());
            packet.extend(self.tx_seq.to_be_bytes());
            packet.extend(self.rx_seq.to_be_bytes());
            self.tx_seq = self.tx_seq.wrapping_add(1);
        }
        packet.extend(header);
        packet.extend(payload);
        let zlp = total.is_multiple_of(self.ep.max_packet_size());
        self.transfer(packet)?;
        // A packet filling whole USB packets needs a zero-length packet to end it.
        if zlp {
            self.transfer(Vec::new())?;
        }
        Ok(())
    }

    fn transfer(&mut self, data: Vec<u8>) -> Result<()> {
        let done = self.ep.transfer_blocking(data.into(), TIMEOUT);
        done.status.map_err(|e| Error::Io(e.into()))
    }
}

struct Reader {
    ep: Endpoint<Bulk, In>,
    buf: Vec<u8>,
}

impl Reader {
    /// Returns the next whole mux packet. With no timeout, fails with
    /// `WouldBlock` after a short wait so the caller can check whether to stop.
    fn next_packet(&mut self, timeout: Option<Duration>) -> Result<Vec<u8>> {
        loop {
            if self.buf.len() >= 8 {
                let len = be32(&self.buf, 4) as usize;
                if !(8..=1 << 24).contains(&len) {
                    return Err(Error::protocol(format!("bad usbmux packet length {len}")));
                }
                if self.buf.len() >= len {
                    let rest = self.buf.split_off(len);
                    return Ok(std::mem::replace(&mut self.buf, rest));
                }
            }
            while self.ep.pending() < 4 {
                let buffer = self.ep.allocate(USB_MRU);
                self.ep.submit(buffer);
            }
            let wait = timeout.unwrap_or(Duration::from_millis(500));
            let Some(done) = self.ep.wait_next_complete(wait) else {
                if timeout.is_some() {
                    return Err(Error::protocol("device didn't answer"));
                }
                return Err(Error::Io(io::ErrorKind::WouldBlock.into()));
            };
            done.status.map_err(|e| Error::Io(e.into()))?;
            self.buf.extend_from_slice(&done.buffer[..done.actual_len]);
        }
    }
}

/// Reads packets and hands them to connections until the device goes away or
/// the mux and all its connections are dropped.
fn read_loop(mut reader: Reader, shared: std::sync::Weak<Shared>) {
    loop {
        let packet = reader.next_packet(None);
        let Some(shared) = shared.upgrade() else {
            return;
        };
        match packet {
            Ok(packet) => handle_packet(&shared, &packet),
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => {
                let mut conns = lock(&shared.conns);
                conns.dead = true;
                for conn in conns.by_port.values() {
                    lock(&conn.state).phase = Phase::Closed;
                    conn.changed.notify_all();
                }
                return;
            }
        }
    }
}

fn handle_packet(shared: &Shared, packet: &[u8]) {
    let protocol = be32(packet, 0);
    let header_len = {
        let mut writer = lock(&shared.writer);
        if writer.version >= 2 && packet.len() >= 16 {
            writer.rx_seq = u16::from_be_bytes([packet[12], packet[13]]);
        }
        mux_header_len(writer.version)
    };
    // Other protocols are device log messages and such.
    if protocol != PROTO_TCP || packet.len() < header_len + TCP_HEADER {
        return;
    }
    let th = &packet[header_len..header_len + TCP_HEADER];
    let payload = &packet[header_len + TCP_HEADER..];
    let dport = u16::from_be_bytes([th[2], th[3]]);
    let seq = be32(th, 4);
    let ack = be32(th, 8);
    let flags = th[13];
    let window = u32::from(u16::from_be_bytes([th[14], th[15]])) << 8;

    let Some(conn) = lock(&shared.conns).by_port.get(&dport).cloned() else {
        return;
    };
    let mut state = lock(&conn.state);
    let reply = |state: &ConnState, flags| {
        let header = tcp_header(
            dport,
            u16::from_be_bytes([th[0], th[1]]),
            state.tx_seq,
            state.tx_ack,
            flags,
        );
        let _ = lock(&shared.writer).send(PROTO_TCP, &header, &[]);
    };
    if flags & TH_RST != 0 {
        state.phase = Phase::Closed;
    } else {
        match state.phase {
            Phase::Connecting if flags == TH_SYN | TH_ACK => {
                state.tx_seq = state.tx_seq.wrapping_add(1);
                state.tx_ack = seq.wrapping_add(1);
                state.peer_ack = ack;
                state.peer_window = window;
                reply(&state, TH_ACK);
                state.phase = Phase::Open;
            }
            Phase::Connecting => state.phase = Phase::Closed,
            Phase::Open => {
                state.peer_ack = ack;
                state.peer_window = window;
                if !payload.is_empty() {
                    state.received.extend(payload);
                    state.tx_ack = state.tx_ack.wrapping_add(payload.len() as u32);
                    reply(&state, TH_ACK);
                }
            }
            Phase::Closed => {}
        }
    }
    conn.changed.notify_all();
}

fn tcp_header(sport: u16, dport: u16, seq: u32, ack: u32, flags: u8) -> [u8; TCP_HEADER] {
    let mut h = [0; TCP_HEADER];
    h[0..2].copy_from_slice(&sport.to_be_bytes());
    h[2..4].copy_from_slice(&dport.to_be_bytes());
    h[4..8].copy_from_slice(&seq.to_be_bytes());
    h[8..12].copy_from_slice(&ack.to_be_bytes());
    h[12] = (TCP_HEADER as u8 / 4) << 4;
    h[13] = flags;
    h[14..16].copy_from_slice(&((RX_WINDOW >> 8) as u16).to_be_bytes());
    h
}

fn version_header(major: u32, minor: u32) -> [u8; 12] {
    let mut h = [0; 12];
    h[0..4].copy_from_slice(&major.to_be_bytes());
    h[4..8].copy_from_slice(&minor.to_be_bytes());
    h
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_tcp_headers() {
        let h = tcp_header(1, 62078, 5, 9, TH_SYN);
        assert_eq!(&h[..4], &[0, 1, 0xf2, 0x7e]);
        assert_eq!(be32(&h, 4), 5);
        assert_eq!(be32(&h, 8), 9);
        assert_eq!(h[12], 0x50);
        assert_eq!(h[13], TH_SYN);
        assert_eq!(&h[14..16], &[0x02, 0x00]);
    }
}
