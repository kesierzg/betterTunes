//! Apple File Conduit, the file service of iPhones and iPod touches.
//!
//! `com.apple.afc` gives access to the media partition (`/var/mobile/Media`,
//! or `/var/root/Media` on iPhone OS 1.x), which holds `iTunes_Control`.
//!
//! Each packet is a 40-byte little-endian header (magic "CFA6LPAA", total
//! length, length of header plus arguments, packet number, operation), the
//! arguments, then any payload. Replies are a status code, data or a file
//! handle.

use crate::{Error, Result, Stream};
use std::collections::BTreeMap;
use std::io::{Read, Write};

pub const SERVICE: &str = "com.apple.afc";

const MAGIC: &[u8; 8] = b"CFA6LPAA";
const HEADER: usize = 40;

const OP_STATUS: u64 = 0x01;
const OP_DATA: u64 = 0x02;
const OP_READ_DIR: u64 = 0x03;
const OP_REMOVE_PATH: u64 = 0x08;
const OP_MAKE_DIR: u64 = 0x09;
const OP_GET_FILE_INFO: u64 = 0x0a;
const OP_GET_DEVICE_INFO: u64 = 0x0b;
const OP_FILE_OPEN: u64 = 0x0d;
const OP_FILE_OPEN_RESULT: u64 = 0x0e;
const OP_FILE_READ: u64 = 0x0f;
const OP_FILE_WRITE: u64 = 0x10;
const OP_FILE_CLOSE: u64 = 0x14;
const OP_RENAME_PATH: u64 = 0x18;
const OP_FILE_LOCK: u64 = 0x1b;

const MODE_READ_ONLY: u64 = 1;
const MODE_READ_WRITE: u64 = 2;
/// Write only, created or truncated.
const MODE_WRITE: u64 = 3;
const READ_CHUNK: u64 = 1 << 16;
const WRITE_CHUNK: usize = 1 << 18;

const LOCK_EXCLUSIVE: u64 = 2 | 4;
const LOCK_UNLOCK: u64 = 8 | 4;
const STATUS_WOULD_BLOCK: u64 = 19;

/// A file or directory found by [`Afc::walk`].
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: String,
    /// `None` for directories.
    pub size: Option<u64>,
}

pub struct Afc {
    stream: Box<dyn Stream>,
    packet_num: u64,
}

impl Afc {
    pub fn new(stream: Box<dyn Stream>) -> Afc {
        Afc {
            stream,
            packet_num: 0,
        }
    }

    /// Names in a directory, without "." and "..".
    pub fn read_dir(&mut self, path: &str) -> Result<Vec<String>> {
        let data = self.request(OP_READ_DIR, &c_string(path), &[])?;
        Ok(strings(&data)
            .filter(|n| *n != "." && *n != "..")
            .map(str::to_owned)
            .collect())
    }

    /// File attributes such as `st_size` and `st_ifmt`, or `None` if the path
    /// doesn't exist.
    pub fn file_info(&mut self, path: &str) -> Result<Option<BTreeMap<String, String>>> {
        match self.request(OP_GET_FILE_INFO, &c_string(path), &[]) {
            Ok(data) => Ok(Some(pairs(&data))),
            Err(Error::Refused(msg)) if msg.contains("not found") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Storage figures such as `FSTotalBytes` and `FSFreeBytes`.
    pub fn device_info(&mut self) -> Result<BTreeMap<String, String>> {
        Ok(pairs(&self.request(OP_GET_DEVICE_INFO, &[], &[])?))
    }

    /// Total size of the files under `path`.
    pub fn disk_usage(&mut self, path: &str) -> Result<u64> {
        Ok(self.walk(path)?.iter().filter_map(|e| e.size).sum())
    }

    /// Everything under `path`, parents before children. Directories have
    /// no size.
    pub fn walk(&mut self, path: &str) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut dirs = vec![path.trim_end_matches('/').to_owned()];
        while let Some(dir) = dirs.pop() {
            let mut names = self.read_dir(&dir)?;
            names.sort();
            for name in names {
                let child = format!("{dir}/{name}");
                let Some(info) = self.file_info(&child)? else {
                    continue;
                };
                let is_dir = info.get("st_ifmt").map(String::as_str) == Some("S_IFDIR");
                let size = info.get("st_size").and_then(|s| s.parse::<u64>().ok());
                if is_dir {
                    dirs.push(child.clone());
                }
                entries.push(Entry {
                    path: child,
                    size: (!is_dir).then(|| size.unwrap_or(0)),
                });
            }
        }
        Ok(entries)
    }

    pub fn make_dir(&mut self, path: &str) -> Result<()> {
        self.request(OP_MAKE_DIR, &c_string(path), &[]).map(drop)
    }

    /// Renames, replacing what's at `to`.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let mut args = c_string(from);
        args.extend(c_string(to));
        self.request(OP_RENAME_PATH, &args, &[]).map(drop)
    }

    /// Writes a file from `data`, replacing it. Returns the bytes written.
    pub fn write_file(&mut self, path: &str, mut data: impl Read) -> Result<u64> {
        let handle = self.open(path, MODE_WRITE)?;
        let mut buf = vec![0; WRITE_CHUNK];
        let mut written = 0;
        let result = loop {
            let n = match data.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => break Err(e.into()),
            };
            if let Err(e) = self.request(OP_FILE_WRITE, &handle.to_le_bytes(), &buf[..n]) {
                break Err(e);
            }
            written += n as u64;
        };
        let closed = self.close(handle);
        result.and(closed).map(|()| written)
    }

    /// Opens `path` for reading and writing, for [`Afc::lock`].
    pub fn open_rw(&mut self, path: &str) -> Result<u64> {
        self.open(path, MODE_READ_WRITE)
    }

    fn open(&mut self, path: &str, mode: u64) -> Result<u64> {
        let mut args = mode.to_le_bytes().to_vec();
        args.extend(c_string(path));
        let (op, reply) = self.exchange(OP_FILE_OPEN, &args, &[])?;
        if op != OP_FILE_OPEN_RESULT || reply.len() < 8 {
            return Err(Error::protocol(format!(
                "unexpected AFC reply {op} to open"
            )));
        }
        Ok(u64::from_le_bytes(reply[..8].try_into().unwrap()))
    }

    pub fn close(&mut self, handle: u64) -> Result<()> {
        self.request(OP_FILE_CLOSE, &handle.to_le_bytes(), &[])
            .map(drop)
    }

    /// Takes an exclusive lock on an open file, or with `false` releases it.
    /// Returns `Ok(false)` if someone else holds it.
    pub fn lock(&mut self, handle: u64, exclusive: bool) -> Result<bool> {
        let mut args = handle.to_le_bytes().to_vec();
        let op = if exclusive {
            LOCK_EXCLUSIVE
        } else {
            LOCK_UNLOCK
        };
        args.extend(op.to_le_bytes());
        match self.request(OP_FILE_LOCK, &args, &[]) {
            Ok(_) => Ok(true),
            Err(Error::Refused(msg)) if msg.ends_with(&status_text(STATUS_WOULD_BLOCK)) => {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Deletes a file or an empty directory.
    pub fn remove(&mut self, path: &str) -> Result<()> {
        self.request(OP_REMOVE_PATH, &c_string(path), &[]).map(drop)
    }

    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>> {
        let handle = self.open(path, MODE_READ_ONLY)?.to_le_bytes().to_vec();
        let mut contents = Vec::new();
        let result = loop {
            let mut args = handle.clone();
            args.extend(READ_CHUNK.to_le_bytes());
            match self.request(OP_FILE_READ, &args, &[]) {
                Ok(chunk) if chunk.is_empty() => break Ok(()),
                Ok(chunk) => contents.extend(chunk),
                Err(e) => break Err(e),
            }
        };
        self.request(OP_FILE_CLOSE, &handle, &[])?;
        result.map(|()| contents)
    }

    /// Sends a request and returns the data it got back, failing on a non-zero
    /// status.
    fn request(&mut self, op: u64, args: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
        match self.exchange(op, args, payload)? {
            (OP_STATUS | OP_DATA, data) => Ok(data),
            (other, _) => Err(Error::protocol(format!("unexpected AFC reply {other}"))),
        }
    }

    fn exchange(&mut self, op: u64, args: &[u8], payload: &[u8]) -> Result<(u64, Vec<u8>)> {
        let this_len = (HEADER + args.len()) as u64;
        let total = this_len + payload.len() as u64;
        let mut packet = Vec::with_capacity(total as usize);
        packet.extend(MAGIC);
        for v in [total, this_len, self.packet_num, op] {
            packet.extend(v.to_le_bytes());
        }
        packet.extend(args);
        packet.extend(payload);
        self.packet_num += 1;
        self.stream.write_all(&packet)?;

        let mut header = [0; HEADER];
        self.stream.read_exact(&mut header)?;
        if &header[..8] != MAGIC {
            return Err(Error::protocol("bad AFC reply magic"));
        }
        let field =
            |i: usize| u64::from_le_bytes(header[8 + i * 8..16 + i * 8].try_into().unwrap());
        let (total, reply_op) = (field(0), field(3));
        if !(HEADER as u64..=1 << 30).contains(&total) {
            return Err(Error::protocol(format!("bad AFC reply length {total}")));
        }
        let mut body = vec![0; total as usize - HEADER];
        self.stream.read_exact(&mut body)?;
        if reply_op == OP_STATUS {
            let code = body
                .get(..8)
                .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
            if code != 0 {
                return Err(Error::Refused(format!("AFC: {}", status_text(code))));
            }
            body.clear();
        }
        Ok((reply_op, body))
    }
}

fn status_text(code: u64) -> String {
    match code {
        7 => "invalid argument".into(),
        8 => "not found".into(),
        9 => "is a directory".into(),
        10 => "permission denied".into(),
        15 => "operation not supported".into(),
        16 => "already exists".into(),
        17 => "busy".into(),
        18 => "no space left".into(),
        STATUS_WOULD_BLOCK => "would block".into(),
        n => format!("error {n}"),
    }
}

fn c_string(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

/// NUL-separated strings.
fn strings(data: &[u8]) -> impl Iterator<Item = &str> {
    data.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok())
}

/// NUL-separated key, value, key, value...
fn pairs(data: &[u8]) -> BTreeMap<String, String> {
    let mut it = strings(data);
    let mut map = BTreeMap::new();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        map.insert(k.to_owned(), v.to_owned());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_value_lists() {
        let map = pairs(b"st_size\x00123\0st_ifmt\0S_IFREG\0");
        assert_eq!(map["st_size"], "123");
        assert_eq!(map["st_ifmt"], "S_IFREG");
        assert_eq!(
            strings(b".\0..\0Music\0").collect::<Vec<_>>(),
            [".", "..", "Music"]
        );
    }
}
