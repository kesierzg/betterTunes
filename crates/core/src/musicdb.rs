//! Reader for Music.app's `Library.musicdb` (macOS 10.15 and later).
//!
//! The format is undocumented; this layout was reverse-engineered.
//! All integers are little-endian.
//!
//! File: an `hfma` header (u32 header length @4, u32 encrypted size @0x54)
//! followed by the body, encrypted and compressed as described in `chunks`.
//!
//! Decoded payload: a run of `hsma` sections (u32 header length @4,
//! u32 section type @12, u32 total length @16), each wrapping one list.
//! Lists hold records (`itma` tracks, `lpma` playlists) whose `boma` children
//! carry strings and other data, identified by their subtype.

use crate::bytes::{f32_at, nonzero, u16_at, u32_at, u64_at};
use crate::chunks::{self, children, list_items, subtype};
use crate::{Error, Library, Playlist, Result, Track};

const SECTION_TRACKS: u32 = 1;
const SECTION_PLAYLISTS: u32 = 2;

/// Track `boma` holding numeric fields; string subtypes are in `chunks`.
const TRACK_NUMERICS: u32 = 1;

// `boma` subtypes inside a playlist.
const PLAYLIST_NAME: u32 = 200;
const PLAYLIST_SMART_RULES: u32 = 201;
const PLAYLIST_ITEM: u32 = 218;

pub fn parse(data: &[u8]) -> Result<Library> {
    parse_payload(&decode(data)?)
}

/// Strips the header, decrypts and decompresses the file.
pub fn decode(data: &[u8]) -> Result<Vec<u8>> {
    if data.get(..4) != Some(b"hfma") {
        return Err(Error::format("missing hfma header"));
    }
    let header_len = u32_at(data, 4).ok_or_else(|| Error::format("truncated header"))? as usize;
    let crypt_size = u32_at(data, 0x54).ok_or_else(|| Error::format("truncated header"))? as usize;
    let body = data
        .get(header_len..)
        .ok_or_else(|| Error::format("header length past end of file"))?;
    chunks::decode_body(body.to_vec(), crypt_size)
}

fn parse_payload(p: &[u8]) -> Result<Library> {
    let mut lib = Library::default();
    let mut off = 0;
    while off < p.len() {
        if p.get(off..off + 4) != Some(b"hsma") {
            return Err(Error::format(format!("expected section at {off:#x}")));
        }
        let header_len = u32_at(p, off + 4).unwrap_or(0) as usize;
        let kind = u32_at(p, off + 12).unwrap_or(0);
        let total_len = u32_at(p, off + 16).unwrap_or(0) as usize;
        let end = off + total_len;
        if total_len < header_len || header_len == 0 || end > p.len() {
            return Err(Error::format(format!("bad section length at {off:#x}")));
        }
        let list = &p[off + header_len..end];
        match kind {
            SECTION_TRACKS => {
                lib.tracks = list_items(list, b"ltma", b"itma")?
                    .into_iter()
                    .map(parse_track)
                    .collect::<Result<_>>()?
            }
            SECTION_PLAYLISTS => {
                lib.playlists = list_items(list, b"lPma", b"lpma")?
                    .into_iter()
                    .map(parse_playlist)
                    .collect::<Result<_>>()?
            }
            _ => {}
        }
        off = end;
    }
    Ok(lib)
}

/// Decodes a string `boma`: u32 encoding @20, u32 byte length @24, text @36.
fn boma_string(boma: &[u8]) -> Option<String> {
    let encoding = u32_at(boma, 20)?;
    let len = u32_at(boma, 24)? as usize;
    let raw = boma.get(36..36usize.checked_add(len)?)?;
    Some(chunks::text(encoding, raw))
}

fn parse_track(rec: &[u8]) -> Result<Track> {
    let mut t = Track {
        persistent_id: u64_at(rec, 16).ok_or_else(|| Error::format("truncated track"))?,
        track_number: nonzero(u16_at(rec, 160)),
        year: nonzero(u32_at(rec, 168)),
        ..Default::default()
    };
    for boma in children(rec)? {
        match subtype(boma) {
            TRACK_NUMERICS => {
                t.sample_rate = chunks::sample_rate(f32_at(boma, 80));
                t.bit_rate = nonzero(u32_at(boma, 108));
                t.duration_ms = nonzero(u32_at(boma, 176));
                t.size = nonzero(u32_at(boma, 316)).map(u64::from);
            }
            sub => {
                if let Some(s) = boma_string(boma) {
                    chunks::set_track_string(&mut t, sub, s);
                }
            }
        }
    }
    Ok(t)
}

fn parse_playlist(rec: &[u8]) -> Result<Playlist> {
    let mut p = Playlist::default();
    for boma in children(rec)? {
        match subtype(boma) {
            PLAYLIST_NAME => p.name = boma_string(boma).unwrap_or_default(),
            PLAYLIST_SMART_RULES => p.is_smart = true,
            // "IPfa" item: track persistent ID @36.
            PLAYLIST_ITEM => p.track_ids.extend(u64_at(boma, 36)),
            _ => {}
        }
    }
    p.is_master = p.name == chunks::MASTER_PLAYLIST_NAME;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunks::build::{encrypt_body, leaf, record, string_leaf, u32s};
    use std::path::PathBuf;

    fn boma_str(sub: u32, encoding: u32, s: &str) -> Vec<u8> {
        string_leaf(b"boma", sub, 20, encoding, s)
    }

    fn section(kind: u32, list_tag: &[u8], records: &[Vec<u8>]) -> Vec<u8> {
        let list = record(list_tag, 92, records, |h| {
            h[8..12].copy_from_slice(&u32s(records.len() as u32));
        });
        let mut s = vec![0; 56];
        s[..4].copy_from_slice(b"hsma");
        s[4..8].copy_from_slice(&u32s(56));
        s[12..16].copy_from_slice(&u32s(kind));
        s[16..20].copy_from_slice(&u32s(56 + list.len() as u32));
        s.extend(list);
        s
    }

    fn sample_library() -> Vec<u8> {
        let mut numerics = vec![0; 368];
        numerics[64..68].copy_from_slice(&44100f32.to_le_bytes());
        numerics[92..96].copy_from_slice(&u32s(128));
        numerics[160..164].copy_from_slice(&u32s(186_409));
        numerics[300..304].copy_from_slice(&u32s(3_749_116));
        let track = record(
            b"itma",
            376,
            &[
                leaf(b"boma", TRACK_NUMERICS, &numerics),
                boma_str(2, 1, "ŻÓŁTY ŚWIT"),
                boma_str(4, 1, "Yellow Lake"),
                boma_str(3, 1, "Debut 2001"),
                boma_str(11, 2, "file:///Music/Yellow%20Lake/01.mp3"),
                leaf(b"boma", 54, b"\0\0\0\0bplist00"),
            ],
            |h| {
                h[16..24].copy_from_slice(&0xFEDC_BA98_7654_3210u64.to_le_bytes());
                h[160..162].copy_from_slice(&5u16.to_le_bytes());
                h[168..172].copy_from_slice(&u32s(2013));
            },
        );
        let mut item = vec![0; 32];
        item[4..8].copy_from_slice(b"IPfa");
        item[20..28].copy_from_slice(&0xFEDC_BA98_7654_3210u64.to_le_bytes());
        let playlist = record(
            b"lpma",
            368,
            &[
                boma_str(PLAYLIST_NAME, 1, "Faves"),
                leaf(b"boma", PLAYLIST_ITEM, &item),
            ],
            |_| {},
        );
        let mut payload = section(3, b"hfma", &[]);
        payload.extend(section(SECTION_TRACKS, b"ltma", &[track]));
        payload.extend(section(SECTION_PLAYLISTS, b"lPma", &[playlist]));

        let body = encrypt_body(&payload, 32);
        let mut file = vec![0; 160];
        file[..4].copy_from_slice(b"hfma");
        file[4..8].copy_from_slice(&u32s(160));
        file[8..12].copy_from_slice(&u32s(160 + body.len() as u32));
        file[0x54..0x58].copy_from_slice(&u32s(32));
        file.extend(body);
        file
    }

    #[test]
    fn parses_tracks_and_playlists() {
        let lib = parse(&sample_library()).unwrap();
        assert_eq!(
            lib.tracks,
            vec![Track {
                persistent_id: 0xFEDC_BA98_7654_3210,
                title: Some("ŻÓŁTY ŚWIT".into()),
                artist: Some("Yellow Lake".into()),
                album: Some("Debut 2001".into()),
                track_number: Some(5),
                year: Some(2013),
                duration_ms: Some(186_409),
                size: Some(3_749_116),
                bit_rate: Some(128),
                sample_rate: Some(44100),
                location: Some(PathBuf::from("/Music/Yellow Lake/01.mp3")),
                ..Default::default()
            }]
        );
        assert_eq!(
            lib.playlists,
            vec![Playlist {
                name: "Faves".into(),
                track_ids: vec![0xFEDC_BA98_7654_3210],
                ..Default::default()
            }]
        );
    }

    #[test]
    fn rejects_corrupt_files() {
        let mut data = sample_library();
        assert!(matches!(parse(&data[..100]), Err(Error::Format(_))));
        let last = data.len() - 1;
        data.truncate(last);
        assert!(parse(&data).is_err());
        assert!(matches!(parse(b"nope"), Err(Error::Format(_))));
    }
}
