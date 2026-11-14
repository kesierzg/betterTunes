//! Client for usbmuxd, the daemon that multiplexes TCP connections to iOS
//! devices over USB.
//!
//! Each message is a 16-byte little-endian header (total length, protocol
//! version 1, message type 8 = plist, tag) followed by an XML plist. After a
//! successful `Connect`, the socket becomes a raw tunnel to the device port.

use crate::{Error, Result};
use plist::{Dictionary, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

const SOCKET_PATH: &str = "/var/run/usbmuxd";
const PLIST_VERSION: u32 = 1;
const PLIST_MESSAGE: u32 = 8;

/// A device attached to usbmuxd.
#[derive(Debug, Clone)]
pub struct Device {
    /// usbmuxd's handle for the device, valid while it stays attached.
    pub id: u64,
    pub udid: String,
    /// "USB" or "Network".
    pub connection: String,
}

pub struct Client {
    stream: UnixStream,
    tag: u32,
}

impl Client {
    pub fn connect() -> Result<Client> {
        let stream = UnixStream::connect(SOCKET_PATH).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("can't reach usbmuxd at {SOCKET_PATH}: {e}"),
            ))
        })?;
        Ok(Client { stream, tag: 0 })
    }

    pub fn list_devices(&mut self) -> Result<Vec<Device>> {
        let reply = self.request("ListDevices", Dictionary::new())?;
        let list = reply
            .get("DeviceList")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::protocol("ListDevices reply has no DeviceList"))?;
        Ok(list
            .iter()
            .filter_map(|d| d.as_dictionary()?.get("Properties")?.as_dictionary())
            .filter_map(|p| {
                Some(Device {
                    id: p.get("DeviceID")?.as_unsigned_integer()?,
                    udid: p.get("SerialNumber")?.as_string()?.to_owned(),
                    connection: p
                        .get("ConnectionType")
                        .and_then(Value::as_string)
                        .unwrap_or("USB")
                        .to_owned(),
                })
            })
            .collect())
    }

    /// The pair record usbmuxd keeps for a device, if it has been paired.
    pub fn read_pair_record(&mut self, udid: &str) -> Result<Option<Vec<u8>>> {
        let mut args = Dictionary::new();
        args.insert("PairRecordID".into(), udid.into());
        let reply = self.request("ReadPairRecord", args)?;
        Ok(reply
            .get("PairRecordData")
            .and_then(Value::as_data)
            .map(<[u8]>::to_vec))
    }

    /// Opens a tunnel to `port` on the device, consuming the client.
    pub fn connect_to(mut self, device: &Device, port: u16) -> Result<UnixStream> {
        let mut args = Dictionary::new();
        args.insert("DeviceID".into(), device.id.into());
        // usbmuxd wants the port in network byte order.
        args.insert("PortNumber".into(), u64::from(port.to_be()).into());
        let reply = self.request("Connect", args)?;
        match reply.get("Number").and_then(Value::as_unsigned_integer) {
            Some(0) => Ok(self.stream),
            Some(3) => Err(Error::Refused(format!(
                "nothing listening on device port {port}"
            ))),
            other => Err(Error::Refused(format!(
                "connect to port {port} failed ({other:?})"
            ))),
        }
    }

    fn request(&mut self, message_type: &str, mut args: Dictionary) -> Result<Dictionary> {
        args.insert("MessageType".into(), message_type.into());
        args.insert("ClientVersionString".into(), "bettertunes".into());
        args.insert("ProgName".into(), "bettertunes".into());
        args.insert("kLibUSBMuxVersion".into(), 3u64.into());
        self.tag += 1;
        self.stream.write_all(&encode(&args, self.tag)?)?;
        let (tag, reply) = decode(&mut self.stream)?;
        if tag != self.tag {
            return Err(Error::protocol(format!(
                "reply tag {tag}, expected {}",
                self.tag
            )));
        }
        Ok(reply)
    }
}

fn encode(msg: &Dictionary, tag: u32) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    plist::to_writer_xml(&mut body, msg)?;
    let mut out = Vec::with_capacity(16 + body.len());
    for v in [16 + body.len() as u32, PLIST_VERSION, PLIST_MESSAGE, tag] {
        out.extend(v.to_le_bytes());
    }
    out.extend(body);
    Ok(out)
}

fn decode(r: &mut impl Read) -> Result<(u32, Dictionary)> {
    let mut header = [0; 16];
    r.read_exact(&mut header)?;
    let field = |i: usize| u32::from_le_bytes(header[i * 4..i * 4 + 4].try_into().unwrap());
    let len = field(0) as usize;
    if !(16..=1 << 24).contains(&len) {
        return Err(Error::protocol(format!("bad message length {len}")));
    }
    let mut body = vec![0; len - 16];
    r.read_exact(&mut body)?;
    let reply: Dictionary = plist::from_bytes(&body)?;
    Ok((field(3), reply))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_messages() {
        let mut msg = Dictionary::new();
        msg.insert("MessageType".into(), "Result".into());
        msg.insert("Number".into(), 0u64.into());
        let bytes = encode(&msg, 7).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize,
            bytes.len()
        );
        assert_eq!(&bytes[4..12], &[1, 0, 0, 0, 8, 0, 0, 0]);
        let (tag, decoded) = decode(&mut &bytes[..]).unwrap();
        assert_eq!(tag, 7);
        assert_eq!(decoded, msg);
    }

    #[test]
    fn rejects_bad_lengths() {
        let mut bytes = encode(&Dictionary::new(), 1).unwrap();
        bytes[..4].copy_from_slice(&3u32.to_le_bytes());
        assert!(matches!(decode(&mut &bytes[..]), Err(Error::Protocol(_))));
    }
}
