//! The iTunesDB of classic iPods and iPhone OS 1.x devices: written for
//! syncing, read back for checking.
//!
//! Every record starts with a four-letter tag, its header length and (for
//! most) its total length, little-endian. The writer produces the layout of
//! iTunes 7.4 (database version 0x19), the oldest iTunes the iPod touch 1G
//! accepts, following libgpod's writer:
//!
//! ```text
//! mhbd                      database (header 244)
//!   mhsd type 1             track list
//!     mhlt                  count
//!       mhit, mhod...       track (header 0x248) and its strings
//!   mhsd type 3, type 2     playlists (3 is the "podcast" copy; no podcasts here)
//!     mhlp                  count
//!       mhyp, mhod...       playlist: name, view settings, and in the master
//!                           playlist the sorted indexes (mhod 52) and jump
//!                           tables (mhod 53)
//!         mhip, mhod 100    item: track ID and position
//! ```
//!
//! Strings (mhod) are UTF-16LE. Times are seconds since 1904.

use crate::{Error, Result};

pub const VERSION: u32 = 0x19;
const MHIT_HEADER: usize = 0x248;

/// Seconds between 1904 (Mac epoch) and 1970.
const MAC_EPOCH_OFFSET: u64 = 2_082_844_800;

pub mod mhod {
    pub const TITLE: u32 = 1;
    pub const LOCATION: u32 = 2;
    pub const ALBUM: u32 = 3;
    pub const ARTIST: u32 = 4;
    pub const GENRE: u32 = 5;
    pub const KIND: u32 = 6;
    pub const COMPOSER: u32 = 12;
    pub const ALBUM_ARTIST: u32 = 22;
    pub const LIBRARY_INDEX: u32 = 52;
    pub const LIBRARY_JUMP_TABLE: u32 = 53;
    /// Playlist view settings, or an item's position.
    pub const PLAYLIST: u32 = 100;
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DbTrack {
    /// Unique within the database; playlists refer to tracks by it.
    pub id: u32,
    /// 64-bit ID; betterTunes uses the iTunes persistent ID.
    pub dbid: u64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    /// E.g. "AAC audio file".
    pub kind: Option<String>,
    /// On the device, colon-separated: ":iTunes_Control:Music:F00:X.m4a".
    pub location: String,
    pub size: u32,
    pub length_ms: u32,
    pub track_number: u32,
    pub track_count: u32,
    pub disc_number: u32,
    pub disc_count: u32,
    pub year: u32,
    /// kbit/s.
    pub bit_rate: u32,
    pub sample_rate: u32,
    /// Unix time.
    pub added: u64,
    /// The cover's image ID in the ArtworkDB and the source image's size.
    pub artwork: Option<(u32, u32)>,
}

impl DbTrack {
    /// The file extension, uppercased and space-padded to four bytes, as
    /// the device wants it: "M4A ", "MP3 ".
    fn filetype(&self) -> u32 {
        let ext = self.location.rsplit('.').next().unwrap_or_default();
        let mut marker = 0u32;
        for i in 0..4 {
            let c = ext.as_bytes().get(i).map_or(b' ', u8::to_ascii_uppercase);
            marker = (marker << 8) | u32::from(c);
        }
        marker
    }

    fn is_mp3(&self) -> bool {
        self.location.to_ascii_lowercase().ends_with(".mp3")
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DbPlaylist {
    pub id: u64,
    pub name: String,
    /// Track IDs, in order.
    pub tracks: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Database {
    pub id: u64,
    /// The name of the master playlist, shown as the device's name.
    pub name: String,
    pub tracks: Vec<DbTrack>,
    /// Ordinary playlists; the master playlist is made from all tracks.
    pub playlists: Vec<DbPlaylist>,
}

pub fn mac_time(unix: u64) -> u32 {
    (unix + MAC_EPOCH_OFFSET).min(u64::from(u32::MAX)) as u32
}

struct Buf(Vec<u8>);

impl Buf {
    fn pos(&self) -> usize {
        self.0.len()
    }
    fn tag(&mut self, tag: &[u8; 4]) {
        self.0.extend(tag);
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend(v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend(v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend(v.to_le_bytes());
    }
    fn zeros(&mut self, n: usize) {
        self.0.resize(self.0.len() + n, 0);
    }
    fn put_u32(&mut self, at: usize, v: u32) {
        self.0[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    /// Writes the total length of the record that started at `start`.
    fn close(&mut self, start: usize) {
        let len = (self.pos() - start) as u32;
        self.put_u32(start + 8, len);
    }
}

impl Database {
    /// The unsigned database; sign it with [`crate::hash58::sign`].
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Buf(Vec::new());
        self.mhbd(&mut b);
        self.track_list(&mut b);
        // The "podcast" playlist list comes before the ordinary one.
        self.playlist_list(&mut b, 3);
        self.playlist_list(&mut b, 2);
        b.close(0);
        b.0
    }

    fn mhbd(&self, b: &mut Buf) {
        b.tag(b"mhbd");
        b.u32(244);
        b.u32(0); // total, later
        b.u32(1);
        b.u32(VERSION);
        b.u32(3); // children
        b.u64(self.id);
        b.u16(1); // platform: Mac
        b.u16(0);
        b.u64(self.id.rotate_left(32)); // unknown ID, repeated in each mhit
        b.u32(0);
        b.u16(0); // hashing scheme, set when signing
        b.zeros(20);
        b.0.extend(b"en"); // language
        b.u64(self.id); // library persistent ID
        b.u32(1);
        b.u32(0x0f);
        b.zeros(20); // hash58
        b.u32(0); // time zone offset
        b.zeros(244 - b.pos());
    }

    fn mhsd(b: &mut Buf, kind: u32) -> usize {
        let start = b.pos();
        b.tag(b"mhsd");
        b.u32(96);
        b.u32(0);
        b.u32(kind);
        b.zeros(80);
        start
    }

    fn track_list(&self, b: &mut Buf) {
        let start = Self::mhsd(b, 1);
        b.tag(b"mhlt");
        b.u32(92);
        b.u32(self.tracks.len() as u32);
        b.zeros(80);
        for t in &self.tracks {
            self.mhit(b, t);
        }
        b.close(start);
    }

    fn mhit(&self, b: &mut Buf, t: &DbTrack) {
        let start = b.pos();
        let mut h = vec![0u8; MHIT_HEADER];
        let mut put32 = |at: usize, v: u32| h[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put32(0x04, MHIT_HEADER as u32);
        put32(0x10, t.id);
        put32(0x14, 1); // visible
        put32(0x18, t.filetype());
        put32(0x20, mac_time(t.added));
        put32(0x24, t.size);
        put32(0x28, t.length_ms);
        put32(0x2c, t.track_number);
        put32(0x30, t.track_count);
        put32(0x34, t.year);
        put32(0x38, t.bit_rate);
        put32(0x3c, t.sample_rate.min(0xFFFF) << 16);
        put32(0x5c, t.disc_number);
        put32(0x60, t.disc_count);
        put32(0x68, mac_time(t.added));
        put32(0x88, (t.sample_rate as f32).to_bits());
        put32(0xd0, 1); // media type: audio
        put32(0x12c, t.size);
        put32(0x168, 1);
        h[..4].copy_from_slice(b"mhit");
        if t.is_mp3() {
            h[0x1c] = 1;
            h[0x1d] = 1;
        }
        h[0x70..0x78].copy_from_slice(&t.dbid.to_le_bytes());
        h[0xa8..0xb0].copy_from_slice(&t.dbid.to_le_bytes());
        h[0x7e..0x80].copy_from_slice(&0xFFFFu16.to_le_bytes());
        let unk144: u16 = if t.is_mp3() { 0x0c } else { 0x33 };
        h[0x90..0x92].copy_from_slice(&unk144.to_le_bytes());
        h[0x124..0x12c].copy_from_slice(&self.id.rotate_left(32).to_le_bytes());
        h[0x134..0x13c].copy_from_slice(&0x8080_8080_8080u64.to_le_bytes());
        if let Some((image, size)) = t.artwork {
            h[0x7c..0x7e].copy_from_slice(&1u16.to_le_bytes()); // artwork count
            h[0x80..0x84].copy_from_slice(&size.to_le_bytes());
            h[0xa4] = 1; // has artwork
            h[0x160..0x164].copy_from_slice(&image.to_le_bytes());
        }
        b.0.extend(h);

        let strings = [
            (mhod::TITLE, t.title.as_deref()),
            (mhod::LOCATION, Some(t.location.as_str())),
            (mhod::ALBUM, t.album.as_deref()),
            (mhod::ARTIST, t.artist.as_deref()),
            (mhod::GENRE, t.genre.as_deref()),
            (mhod::KIND, t.kind.as_deref()),
            (mhod::COMPOSER, t.composer.as_deref()),
            (mhod::ALBUM_ARTIST, t.album_artist.as_deref()),
        ];
        let mut count = 0;
        for (kind, s) in strings {
            if let Some(s) = s.filter(|s| !s.is_empty()) {
                string_mhod(b, kind, s);
                count += 1;
            }
        }
        b.put_u32(start + 12, count);
        b.close(start);
    }

    fn playlist_list(&self, b: &mut Buf, kind: u32) {
        let start = Self::mhsd(b, kind);
        b.tag(b"mhlp");
        b.u32(92);
        b.u32(1 + self.playlists.len() as u32);
        b.zeros(80);
        let master = DbPlaylist {
            id: self.id ^ 0x4D41_5354_4552, // "MASTER"
            name: self.name.clone(),
            tracks: self.tracks.iter().map(|t| t.id).collect(),
        };
        self.mhyp(b, &master, true);
        for p in &self.playlists {
            self.mhyp(b, p, false);
        }
        b.close(start);
    }

    fn mhyp(&self, b: &mut Buf, p: &DbPlaylist, master: bool) {
        let start = b.pos();
        b.tag(b"mhyp");
        b.u32(108);
        b.u32(0);
        b.u32(if master { 12 } else { 2 }); // mhods
        b.u32(p.tracks.len() as u32);
        b.u8(u8::from(master));
        b.zeros(3);
        b.u32(0); // timestamp
        b.u64(p.id);
        b.u32(0);
        b.u16(1); // string mhods
        b.u16(0); // not podcasts
        b.u32(1); // sort order: manual
        b.zeros(60);
        string_mhod(b, mhod::TITLE, &p.name);
        view_settings_mhod(b);
        if master {
            self.library_indexes(b);
        }
        for (pos, &id) in p.tracks.iter().enumerate() {
            let item = b.pos();
            b.tag(b"mhip");
            b.u32(76);
            b.u32(0);
            b.u32(1); // children
            b.u32(0);
            b.u32(0);
            b.u32(id);
            b.zeros(4 + 4 + 40);
            // The position record counts as part of the item.
            b.tag(b"mhod");
            b.u32(24);
            b.u32(44);
            b.u32(mhod::PLAYLIST);
            b.zeros(8);
            b.u32(pos as u32);
            b.zeros(16);
            b.close(item);
        }
        b.close(start);
    }

    /// The master playlist's indexes: track positions sorted by title,
    /// artist, album, genre and composer (mhod 52), each followed by a
    /// table of where each first letter starts (mhod 53).
    fn library_indexes(&self, b: &mut Buf) {
        let key = |s: &Option<String>| s.as_deref().unwrap_or_default().to_lowercase();
        let sorted = |order: &dyn Fn(&DbTrack) -> Vec<String>| {
            let mut idx: Vec<usize> = (0..self.tracks.len()).collect();
            idx.sort_by_cached_key(|&i| {
                let t = &self.tracks[i];
                (order(t), t.disc_number, t.track_number, key(&t.title))
            });
            idx
        };
        // Sort type, sorted track positions, and the field the jump table uses.
        type Index = (u32, Vec<usize>, fn(&DbTrack) -> &Option<String>);
        let indexes: [Index; 5] = [
            (0x03, sorted(&|t| vec![key(&t.title)]), |t| &t.title),
            (
                0x05,
                sorted(&|t| vec![key(&t.artist), key(&t.album)]),
                |t| &t.artist,
            ),
            (0x04, sorted(&|t| vec![key(&t.album)]), |t| &t.album),
            (
                0x07,
                sorted(&|t| vec![key(&t.genre), key(&t.artist), key(&t.album)]),
                |t| &t.genre,
            ),
            (
                0x12,
                sorted(&|t| vec![key(&t.composer), key(&t.album)]),
                |t| &t.composer,
            ),
        ];
        for (sort, order, field) in indexes {
            b.tag(b"mhod");
            b.u32(24);
            b.u32(72 + 4 * order.len() as u32);
            b.u32(mhod::LIBRARY_INDEX);
            b.zeros(8);
            b.u32(sort);
            b.u32(order.len() as u32);
            b.zeros(40);
            for &i in &order {
                b.u32(i as u32);
            }
            // Runs of the same first letter, in sorted order.
            let mut runs: Vec<(u16, u32, u32)> = Vec::new();
            for (n, &i) in order.iter().enumerate() {
                let letter = jump_letter(field(&self.tracks[i]).as_deref().unwrap_or_default());
                match runs.last_mut() {
                    Some(run) if run.0 == letter => run.2 += 1,
                    _ => runs.push((letter, n as u32, 1)),
                }
            }
            b.tag(b"mhod");
            b.u32(24);
            b.u32(40 + 12 * runs.len() as u32);
            b.u32(mhod::LIBRARY_JUMP_TABLE);
            b.zeros(8);
            b.u32(sort);
            b.u32(runs.len() as u32);
            b.zeros(8);
            for (letter, start, count) in runs {
                b.u16(letter);
                b.u16(0);
                b.u32(start);
                b.u32(count);
            }
        }
    }
}

/// The first letter or digit of `s`, uppercased; '0' for digits and for
/// strings with neither.
fn jump_letter(s: &str) -> u16 {
    match s.chars().find(|c| c.is_alphanumeric()) {
        Some(c) if c.is_alphabetic() => {
            let upper = c.to_uppercase().next().unwrap_or(c);
            let mut units = [0u16; 2];
            upper.encode_utf16(&mut units)[0]
        }
        _ => u16::from(b'0'),
    }
}

fn string_mhod(b: &mut Buf, kind: u32, s: &str) {
    let utf16: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
    b.tag(b"mhod");
    b.u32(24);
    b.u32(40 + utf16.len() as u32);
    b.u32(kind);
    b.zeros(8);
    b.u32(1); // UTF-16
    b.u32(utf16.len() as u32);
    b.u32(1);
    b.u32(0);
    b.0.extend(utf16);
}

/// The playlist view settings iTunes writes (column layout); without them
/// iTunes shows the playlist empty. Values from libgpod.
fn view_settings_mhod(b: &mut Buf) {
    let start = b.pos();
    b.tag(b"mhod");
    b.u32(0x18);
    b.u32(0x288);
    b.u32(mhod::PLAYLIST);
    b.zeros(24);
    for v in [0x010084, 0x05, 0x09, 0x03, 0x120001] {
        b.u32(v);
    }
    for v in [0xc80002, 0x3c000d, 0x7d0004, 0x7d0003, 0x640008] {
        b.zeros(12);
        b.u32(v);
    }
    b.zeros(12);
    for v in [0x640017, 0x500014, 0x7d0015] {
        b.u32(v);
        b.u32(1);
        b.zeros(8);
    }
    b.zeros(0x288 - (b.pos() - start));
}

/// Reads a device's "Play Counts" file: what was played, skipped and rated
/// since the iTunesDB was written, one entry per track in database order.
/// Entries are 12 to 28 bytes depending on the firmware: plays, last played,
/// bookmark, then rating, an unknown field, skips and last skipped.
pub fn parse_play_counts(data: &[u8]) -> Result<Vec<crate::pull::Plays>> {
    let u32_at = |at: usize| {
        data.get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    if !data.starts_with(b"mhdp") {
        return Err(Error::format("not a Play Counts file"));
    }
    let (header, entry, count) = match (u32_at(4), u32_at(8), u32_at(12)) {
        (Some(h), Some(e), Some(n)) if e >= 12 => (h as usize, e as usize, n as usize),
        _ => return Err(Error::format("bad Play Counts header")),
    };
    (0..count)
        .map(|i| {
            let at = header + i * entry;
            let field = |off: usize| {
                if off + 4 <= entry {
                    u32_at(at + off)
                } else {
                    Some(0)
                }
            };
            let plays = field(0).ok_or_else(|| Error::format("Play Counts truncated"))?;
            Ok(crate::pull::Plays {
                plays,
                skips: field(20).unwrap_or(0),
                last_played: field(4).filter(|&t| t != 0),
                rating: field(12)
                    .and_then(|r| u8::try_from(r).ok())
                    .filter(|&r| r > 0),
            })
        })
        .collect()
}

/// Reads a database: tracks and playlists (the master playlist included,
/// first).
pub fn parse(data: &[u8]) -> Result<Database> {
    let u32_at = |at: usize| -> Result<u32> {
        data.get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .ok_or_else(|| Error::format("iTunesDB truncated"))
    };
    if !data.starts_with(b"mhbd") {
        return Err(Error::format("not an iTunesDB"));
    }
    let mut db = Database {
        id: u64::from_le_bytes(data[0x18..0x20].try_into().unwrap()),
        ..Database::default()
    };
    let mut at = u32_at(4)? as usize;
    while at < data.len() {
        let (header, total, kind) = (
            u32_at(at + 4)? as usize,
            u32_at(at + 8)? as usize,
            u32_at(at + 12)?,
        );
        if &data[at..at + 4] != b"mhsd" || total < header {
            return Err(Error::format("bad mhsd"));
        }
        let list = at + header;
        let list_header = u32_at(list + 4)? as usize;
        let count = u32_at(list + 8)?;
        let mut item = list + list_header;
        for _ in 0..count {
            let item_total = u32_at(item + 8)? as usize;
            match (kind, &data[item..item + 4]) {
                (1, b"mhit") => db.tracks.push(parse_mhit(&data[item..item + item_total])?),
                (2, b"mhyp") => {
                    let p = parse_mhyp(&data[item..item + item_total])?;
                    if data[item + 20] == 1 {
                        db.name = p.name.clone();
                    }
                    db.playlists.push(p);
                }
                _ => {}
            }
            item += item_total;
        }
        at += total;
    }
    Ok(db)
}

fn children(rec: &[u8], from: usize) -> impl Iterator<Item = &[u8]> {
    let mut at = from;
    std::iter::from_fn(move || {
        let total = rec.get(at + 8..at + 12)?;
        let total = u32::from_le_bytes(total.try_into().unwrap()) as usize;
        let child = rec.get(at..at + total.max(12))?;
        at += total.max(12);
        Some(child)
    })
}

fn mhod_string(m: &[u8]) -> Option<String> {
    let len = u32::from_le_bytes(m.get(28..32)?.try_into().unwrap()) as usize;
    Some(crate::bytes::utf16le(m.get(40..40 + len)?))
}

fn parse_mhit(rec: &[u8]) -> Result<DbTrack> {
    let u32_at = |at: usize| u32::from_le_bytes(rec[at..at + 4].try_into().unwrap());
    let header = u32_at(4) as usize;
    let mut t = DbTrack {
        id: u32_at(0x10),
        size: u32_at(0x24),
        length_ms: u32_at(0x28),
        track_number: u32_at(0x2c),
        track_count: u32_at(0x30),
        year: u32_at(0x34),
        bit_rate: u32_at(0x38),
        sample_rate: u32_at(0x3c) >> 16,
        disc_number: u32_at(0x5c),
        disc_count: u32_at(0x60),
        dbid: u64::from_le_bytes(rec[0x70..0x78].try_into().unwrap()),
        added: u64::from(u32_at(0x68)).saturating_sub(MAC_EPOCH_OFFSET),
        artwork: (rec[0xa4] == 1).then(|| (u32_at(0x160), u32_at(0x80))),
        ..DbTrack::default()
    };
    for m in children(rec, header).filter(|m| m.starts_with(b"mhod")) {
        let kind = u32::from_le_bytes(m[12..16].try_into().unwrap());
        let s = mhod_string(m);
        match kind {
            mhod::TITLE => t.title = s,
            mhod::LOCATION => t.location = s.unwrap_or_default(),
            mhod::ALBUM => t.album = s,
            mhod::ARTIST => t.artist = s,
            mhod::GENRE => t.genre = s,
            mhod::KIND => t.kind = s,
            mhod::COMPOSER => t.composer = s,
            mhod::ALBUM_ARTIST => t.album_artist = s,
            _ => {}
        }
    }
    Ok(t)
}

fn parse_mhyp(rec: &[u8]) -> Result<DbPlaylist> {
    let header = u32::from_le_bytes(rec[4..8].try_into().unwrap()) as usize;
    let mut p = DbPlaylist {
        id: u64::from_le_bytes(rec[28..36].try_into().unwrap()),
        ..DbPlaylist::default()
    };
    for child in children(rec, header) {
        match &child[..4] {
            b"mhod" if u32::from_le_bytes(child[12..16].try_into().unwrap()) == mhod::TITLE => {
                p.name = mhod_string(child).unwrap_or_default();
            }
            b"mhip" => p
                .tracks
                .push(u32::from_le_bytes(child[24..28].try_into().unwrap())),
            _ => {}
        }
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> Database {
        let track = |id, title: &str, artist: &str, ext: &str| DbTrack {
            id,
            dbid: 0x1000_2000_3000_4000 + u64::from(id),
            title: Some(title.into()),
            artist: Some(artist.into()),
            album: Some("Debut".into()),
            kind: Some("AAC audio file".into()),
            location: format!(":iTunes_Control:Music:F01:{id}.{ext}"),
            size: 4_000_000,
            length_ms: 200_000,
            bit_rate: 256,
            sample_rate: 44_100,
            added: 1_700_000_000,
            artwork: (id == 100).then_some((100, 96_999)),
            ..DbTrack::default()
        };
        Database {
            id: 0x1122_3344_5566_7788,
            name: "My iPod".into(),
            tracks: vec![
                track(100, "Night Drive", "Yellow Lake", "m4a"),
                track(101, "ǅungla", "Ćma Trio", "mp3"),
                track(102, "2 Steps", "Anna Lind", "m4a"),
            ],
            playlists: vec![DbPlaylist {
                id: 0x1111_2222_3333_4444,
                name: "★ night ☾".into(),
                tracks: vec![102, 100],
            }],
        }
    }

    #[test]
    fn writes_what_it_reads_back() {
        let db = database();
        let bytes = db.to_bytes();
        assert_eq!(
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize,
            bytes.len()
        );
        let back = parse(&bytes).unwrap();
        assert_eq!(back.tracks, db.tracks);
        assert_eq!(back.name, db.name);
        // The master playlist comes first, with every track.
        assert_eq!(back.playlists[0].tracks, [100, 101, 102]);
        assert_eq!(back.playlists[1], db.playlists[0]);
    }

    #[test]
    fn records_have_the_sizes_devices_expect() {
        let bytes = database().to_bytes();
        let find = |tag: &[u8]| bytes.windows(4).position(|w| w == tag).unwrap();
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!(u32_at(4), 244);
        assert_eq!(u32_at(0x10), VERSION);
        assert_eq!(u32_at(find(b"mhsd") + 4), 96);
        assert_eq!(u32_at(find(b"mhit") + 4), 0x248);
        let mhit = find(b"mhit");
        assert_eq!(&bytes[mhit + 0x18..mhit + 0x1c], b" A4M"); // "M4A " as a u32
        let mhyp = find(b"mhyp");
        assert_eq!(u32_at(mhyp + 4), 108);
        // Name, then the 0x288-byte view settings.
        let settings = mhyp + 108 + u32_at(mhyp + 108 + 8) as usize;
        assert_eq!(u32_at(settings + 8), 0x288);
        assert_eq!(u32_at(settings + 12), mhod::PLAYLIST);
        assert_eq!(u32_at(find(b"mhip") + 8), 76 + 44);
    }

    #[test]
    fn reads_play_counts() {
        let mut data = b"mhdp".to_vec();
        for v in [0x60u32, 0x1c, 2] {
            data.extend(v.to_le_bytes());
        }
        data.resize(0x60, 0);
        for entry in [[3u32, 3_900_000_000, 0, 60, 0, 1, 0], [0; 7]] {
            for v in entry {
                data.extend(v.to_le_bytes());
            }
        }
        let plays = parse_play_counts(&data).unwrap();
        assert_eq!(plays[0].plays, 3);
        assert_eq!(plays[0].skips, 1);
        assert_eq!(plays[0].rating, Some(60));
        assert_eq!(plays[0].last_played, Some(3_900_000_000));
        assert_eq!(plays[1], crate::pull::Plays::default());
    }

    #[test]
    fn jump_letters_follow_libgpod() {
        assert_eq!(jump_letter("yoshi"), u16::from(b'Y'));
        assert_eq!(jump_letter("2 Steps"), u16::from(b'0'));
        assert_eq!(jump_letter("...ćpaj"), 'Ć' as u16);
        assert_eq!(jump_letter("☆ ✮"), u16::from(b'0'));
    }
}
