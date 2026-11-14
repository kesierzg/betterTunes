//! Reader for iTunes' `iTunes Library.itl` (verified with iTunes 12.8; the
//! layout likely dates back to iTunes 10).
//!
//! The format is undocumented. This layout was reverse-engineered from an
//! iTunes 12.8 library and checked field by field against the XML export of
//! the same library.
//!
//! File: an `hdfm` header, big-endian (u32 header length @4, u32 encrypted
//! size @0x5C), followed by the body, encrypted and compressed as described in
//! `chunks`.
//!
//! Decoded payload (little-endian): a run of `msdh` sections (u32 header
//! length @4, u32 total length @8, u32 section type @12), each wrapping one
//! list. Lists hold `mith` tracks and `miph` playlists; their `mhoh` children
//! carry strings, and playlists also have one `mtph` child per item.

use crate::bytes::{f32_at, nonzero, u16_at, u32_at, u64_at};
use crate::chunks::{self, children, list_items, subtype};
use crate::{Error, Library, Playlist, Result, Track};
use std::collections::HashMap;

const SECTION_TRACKS: u32 = 1;
const SECTION_PLAYLISTS: u32 = 2;

// `mhoh` subtypes inside a playlist; track subtypes are in `chunks`.
const PLAYLIST_NAME: u32 = 100;
const PLAYLIST_SMART_INFO: u32 = 101;
const PLAYLIST_SMART_CRITERIA: u32 = 102;

pub fn parse(data: &[u8]) -> Result<Library> {
    parse_payload(&decode(data)?)
}

/// Strips the header, decrypts and decompresses the file.
pub fn decode(data: &[u8]) -> Result<Vec<u8>> {
    if data.get(..4) != Some(b"hdfm") {
        return Err(Error::format("missing hdfm header"));
    }
    let be32 = |off: usize| {
        u32_at(data, off)
            .map(u32::swap_bytes)
            .ok_or_else(|| Error::format("truncated header"))
    };
    let header_len = be32(4)? as usize;
    let crypt_size = be32(0x5C)? as usize;
    let body = data
        .get(header_len..)
        .ok_or_else(|| Error::format("header length past end of file"))?;
    chunks::decode_body(body.to_vec(), crypt_size)
}

fn parse_payload(p: &[u8]) -> Result<Library> {
    if p.get(..4) != Some(b"msdh") {
        return Err(Error::Unsupported(
            "this .itl is from an iTunes version older than 10; export the \
             library as XML from iTunes and open that instead"
                .into(),
        ));
    }
    let mut lib = Library::default();
    // Playlist items reference tracks by their library-local track ID.
    let mut ids = HashMap::new();
    let mut playlists = Vec::new();
    for section in chunks::records(p)? {
        if &section[..4] != b"msdh" {
            return Err(Error::format("expected msdh section"));
        }
        let header_len = u32_at(section, 4).unwrap_or(0) as usize;
        let list = section
            .get(header_len..)
            .ok_or_else(|| Error::format("section header past end"))?;
        match subtype(section) {
            SECTION_TRACKS => {
                for rec in list_items(list, b"mlth", b"mith")? {
                    let (id, track) = parse_track(rec)?;
                    ids.insert(id, track.persistent_id);
                    lib.tracks.push(track);
                }
            }
            SECTION_PLAYLISTS => playlists = list_items(list, b"mlph", b"miph")?,
            _ => {}
        }
    }
    lib.playlists = playlists
        .into_iter()
        .map(|rec| parse_playlist(rec, &ids))
        .collect::<Result<_>>()?;
    Ok(lib)
}

/// Decodes a string `mhoh`: u32 encoding @24, u32 byte length @28, text @40.
fn mhoh_string(mhoh: &[u8]) -> Option<String> {
    let encoding = u32_at(mhoh, 24)?;
    let len = u32_at(mhoh, 28)? as usize;
    let raw = mhoh.get(40..40usize.checked_add(len)?)?;
    Some(chunks::text(encoding, raw))
}

/// Returns the track and its library-local track ID.
fn parse_track(rec: &[u8]) -> Result<(u32, Track)> {
    let id = u32_at(rec, 16).ok_or_else(|| Error::format("truncated track"))?;
    let mut t = Track {
        persistent_id: u64_at(rec, 128).ok_or_else(|| Error::format("truncated track"))?,
        size: nonzero(u32_at(rec, 36)).map(u64::from),
        duration_ms: nonzero(u32_at(rec, 40)),
        track_number: nonzero(u16_at(rec, 44)),
        track_count: nonzero(u16_at(rec, 48)),
        year: nonzero(u32_at(rec, 52)),
        bit_rate: nonzero(u32_at(rec, 56)),
        disc_number: nonzero(u16_at(rec, 104)),
        disc_count: nonzero(u16_at(rec, 106)),
        sample_rate: chunks::sample_rate(f32_at(rec, 152)),
        play_count: u32_at(rec, 76).unwrap_or(0),
        last_played: nonzero(u32_at(rec, 100)),
        rating: nonzero(rec.get(108).copied()),
        skip_count: u32_at(rec, 216).unwrap_or(0),
        ..Default::default()
    };
    for mhoh in children(rec)?.into_iter().filter(|r| &r[..4] == b"mhoh") {
        if let Some(s) = mhoh_string(mhoh) {
            chunks::set_track_string(&mut t, subtype(mhoh), s);
        }
    }
    Ok((id, t))
}

fn parse_playlist(rec: &[u8], ids: &HashMap<u32, u64>) -> Result<Playlist> {
    let mut p = Playlist {
        persistent_id: u64_at(rec, 440),
        // Byte 569, unaligned: matches the XML export's "Distinguished Kind".
        distinguished_kind: rec.get(569).copied().unwrap_or(0),
        ..Default::default()
    };
    for child in children(rec)? {
        match (&child[..4], subtype(child)) {
            (b"mhoh", PLAYLIST_NAME) => p.name = mhoh_string(child).unwrap_or_default(),
            (b"mhoh", PLAYLIST_SMART_INFO | PLAYLIST_SMART_CRITERIA) => p.is_smart = true,
            // Item: track ID @24.
            (b"mtph", _) => p
                .track_ids
                .extend(u32_at(child, 24).and_then(|id| ids.get(&id).copied())),
            _ => {}
        }
    }
    p.is_master = p.name == chunks::MASTER_PLAYLIST_NAME;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunks::build::{encrypt_body, record, string_leaf, u32s};
    use std::path::PathBuf;

    fn mhoh(sub: u32, encoding: u32, s: &str) -> Vec<u8> {
        string_leaf(b"mhoh", sub, 24, encoding, s)
    }

    fn section(kind: u32, list_tag: &[u8], records: &[Vec<u8>]) -> Vec<u8> {
        let list = record(list_tag, 92, records, |h| {
            h[8..12].copy_from_slice(&u32s(records.len() as u32));
        });
        record(b"msdh", 96, &[list], |h| {
            h[12..16].copy_from_slice(&u32s(kind))
        })
    }

    fn sample_library() -> Vec<u8> {
        let track = record(
            b"mith",
            756,
            &[
                mhoh(2, 3, "Håstad"),
                mhoh(4, 1, "Młody Bóg"),
                mhoh(27, 1, "Aloe Boyz"),
                mhoh(
                    11,
                    2,
                    "file:///Users/g/Music/iTunes/Music/Aloe%20Boyz/01.m4a",
                ),
            ],
            |h| {
                h[16..20].copy_from_slice(&u32s(4301));
                h[36..40].copy_from_slice(&u32s(14_762_800));
                h[40..44].copy_from_slice(&u32s(114_460));
                h[44..46].copy_from_slice(&2u16.to_le_bytes());
                h[48..50].copy_from_slice(&10u16.to_le_bytes());
                h[52..56].copy_from_slice(&u32s(2019));
                h[56..60].copy_from_slice(&u32s(1411));
                h[104..106].copy_from_slice(&1u16.to_le_bytes());
                h[128..136].copy_from_slice(&0xAAAA_BBBB_CCCC_0001u64.to_le_bytes());
                h[152..156].copy_from_slice(&44100f32.to_le_bytes());
            },
        );
        let item = |track_id: u32| {
            record(b"mtph", 84, &[], move |h| {
                h[24..28].copy_from_slice(&u32s(track_id))
            })
        };
        let master = record(
            b"miph",
            3500,
            &[mhoh(PLAYLIST_NAME, 3, "####!####"), item(4301)],
            |h| h[440..448].copy_from_slice(&0x0123_4567_89AB_CDEFu64.to_le_bytes()),
        );
        let smart = record(
            b"miph",
            3500,
            &[
                mhoh(PLAYLIST_NAME, 1, "☆ 5 star"),
                mhoh(PLAYLIST_SMART_INFO, 0, ""),
                item(4301),
                item(999),
            ],
            |_| {},
        );
        let mut payload = section(3, b"mfdh", &[]);
        payload.extend(section(SECTION_TRACKS, b"mlth", &[track]));
        payload.extend(section(SECTION_PLAYLISTS, b"mlph", &[master, smart]));

        let body = encrypt_body(&payload, 102_400);
        let mut file = vec![0; 144];
        file[..4].copy_from_slice(b"hdfm");
        file[4..8].copy_from_slice(&144u32.to_be_bytes());
        file[8..12].copy_from_slice(&(144 + body.len() as u32).to_be_bytes());
        file[0x5C..0x60].copy_from_slice(&102_400u32.to_be_bytes());
        file.extend(body);
        file
    }

    #[test]
    fn parses_tracks_and_playlists() {
        let lib = parse(&sample_library()).unwrap();
        assert_eq!(
            lib.tracks,
            vec![Track {
                persistent_id: 0xAAAA_BBBB_CCCC_0001,
                title: Some("Håstad".into()),
                artist: Some("Młody Bóg".into()),
                album_artist: Some("Aloe Boyz".into()),
                track_number: Some(2),
                track_count: Some(10),
                disc_number: Some(1),
                year: Some(2019),
                duration_ms: Some(114_460),
                size: Some(14_762_800),
                bit_rate: Some(1411),
                sample_rate: Some(44100),
                location: Some(PathBuf::from(
                    "/Users/g/Music/iTunes/Music/Aloe Boyz/01.m4a"
                )),
                ..Default::default()
            }]
        );
        let [master, smart] = &lib.playlists[..] else {
            panic!("expected 2 playlists, got {:?}", lib.playlists)
        };
        assert!(master.is_master && !master.is_smart);
        assert_eq!(master.persistent_id, Some(0x0123_4567_89AB_CDEF));
        assert_eq!(master.track_ids, vec![0xAAAA_BBBB_CCCC_0001]);
        assert_eq!(smart.name, "☆ 5 star");
        assert!(smart.is_smart && !smart.is_master);
        // Item 999 references no track and is dropped.
        assert_eq!(smart.track_ids, vec![0xAAAA_BBBB_CCCC_0001]);
    }

    #[test]
    fn rejects_corrupt_and_old_files() {
        let data = sample_library();
        assert!(matches!(parse(&data[..100]), Err(Error::Format(_))));
        assert!(parse(&data[..data.len() - 1]).is_err());
        assert!(matches!(parse(b"hdsm...."), Err(Error::Format(_))));

        // Pre-iTunes 10 payloads don't start with msdh.
        let mut old = data[..144].to_vec();
        old[0x5C..0x60].fill(0);
        old.extend(encrypt_body(b"hdsm\0\0\0\x60", u32::MAX));
        assert!(matches!(parse(&old), Err(Error::Unsupported(_))));
    }
}
