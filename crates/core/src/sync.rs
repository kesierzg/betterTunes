//! What to put on a device: the user's selection, the conversion setting, and
//! the plan worked out from them.
//!
//! The selection is a union: playlists (all their tracks), artists, albums
//! and genres, or the whole library. Conversion follows iTunes' "Convert
//! higher bit rate songs to AAC": a track is re-encoded when its bit rate is
//! above the target or it is lossless. Formats iPods can't play (FLAC, Ogg
//! and so on) are always converted, at the target rate or 256 kbps.

use crate::{Library, Playlist, Track};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Bit rates offered for AAC conversion, in kbit/s.
pub const AAC_RATES: [u32; 6] = [128, 160, 192, 224, 256, 320];
/// Rate for formats that must be converted when conversion is off.
pub const FALLBACK_KBPS: u32 = 256;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Selection {
    /// The whole library; the lists below are then ignored.
    pub everything: bool,
    pub playlists: Vec<PlaylistRef>,
    pub artists: Vec<String>,
    pub albums: Vec<AlbumRef>,
    pub genres: Vec<String>,
}

/// A playlist, by persistent ID where the library has one, else by name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PlaylistRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_id: Option<String>,
    pub name: String,
}

impl PlaylistRef {
    pub fn new(p: &Playlist) -> PlaylistRef {
        PlaylistRef {
            persistent_id: p.persistent_id.map(|id| format!("{id:016X}")),
            name: p.name.clone(),
        }
    }

    pub fn matches(&self, p: &Playlist) -> bool {
        match (&self.persistent_id, p.persistent_id) {
            (Some(id), Some(pid)) => u64::from_str_radix(id, 16) == Ok(pid),
            _ => self.name == p.name,
        }
    }
}

/// An album, by name and album artist (or artist when there's none).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AlbumRef {
    pub album: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
}

impl AlbumRef {
    pub fn matches(&self, t: &Track) -> bool {
        t.album.as_deref() == Some(&self.album)
            && self
                .artist
                .as_ref()
                .is_none_or(|a| album_artist(t) == Some(a.as_str()))
    }
}

fn album_artist(t: &Track) -> Option<&str> {
    t.album_artist.as_deref().or(t.artist.as_deref())
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        !self.everything
            && self.playlists.is_empty()
            && self.artists.is_empty()
            && self.albums.is_empty()
            && self.genres.is_empty()
    }

    /// Selected playlists that are in the library, and the references that
    /// no longer match anything.
    pub fn resolve_playlists<'a>(
        &'a self,
        lib: &'a Library,
    ) -> (Vec<&'a Playlist>, Vec<&'a PlaylistRef>) {
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for r in &self.playlists {
            match lib.playlists.iter().find(|p| !p.is_master && r.matches(p)) {
                Some(p) => found.push(p),
                None => missing.push(r),
            }
        }
        (found, missing)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Conversion {
    /// Copy files as they are (unplayable formats are still converted).
    #[default]
    Off,
    /// Re-encode lossless files and anything above this many kbit/s to AAC.
    Aac { kbps: u32 },
}

/// Audio formats as far as syncing cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Mp3,
    Aac,
    /// Apple Lossless, WAV, AIFF: playable, but large.
    Lossless,
    /// FairPlay-protected (.m4p): plays only on authorised devices, and
    /// Apple Music downloads not at all.
    Protected,
    Video,
    /// Audio iPods can't play, such as FLAC or Ogg.
    Unplayable,
}

impl Format {
    pub fn of(t: &Track) -> Option<Format> {
        let path = t.location.as_ref()?;
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "mp3" => Format::Mp3,
            // ALAC shares the .m4a container; lossless rates are far above
            // any AAC encoding.
            "m4a" | "m4b" | "aac" => {
                let lossless_kind = t.kind.as_deref().is_some_and(|k| k.contains("Lossless"));
                if lossless_kind || t.bit_rate.is_some_and(|r| r > 500) {
                    Format::Lossless
                } else {
                    Format::Aac
                }
            }
            "wav" | "aif" | "aiff" | "aifc" => Format::Lossless,
            "m4p" => Format::Protected,
            "mp4" | "m4v" | "mov" => Format::Video,
            _ => Format::Unplayable,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Copy,
    Convert { kbps: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Cloud or streaming track with no local file.
    NoFile,
    /// The file isn't where the library says.
    Missing,
    Protected,
    Video,
}

impl Skip {
    pub fn describe(self) -> &'static str {
        match self {
            Skip::NoFile => "not downloaded (no local file)",
            Skip::Missing => "file missing",
            Skip::Protected => "DRM-protected (.m4p)",
            Skip::Video => "video",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Item<'a> {
    pub track: &'a Track,
    pub action: Action,
    /// Bytes on the device: the file's size, or an estimate after conversion.
    pub size: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Plan<'a> {
    pub items: Vec<Item<'a>>,
    pub skipped: Vec<(&'a Track, Skip)>,
    /// Playlists to create on the device, with only the planned tracks.
    pub playlists: Vec<(&'a Playlist, Vec<u64>)>,
    pub missing_playlists: Vec<&'a PlaylistRef>,
}

impl<'a> Plan<'a> {
    pub fn total_size(&self) -> u64 {
        self.items.iter().map(|i| i.size).sum()
    }

    pub fn converted(&self) -> impl Iterator<Item = &Item<'a>> {
        self.items
            .iter()
            .filter(|i| matches!(i.action, Action::Convert { .. }))
    }
}

/// Rewrites library paths (e.g. `/Users/me` to `/Volumes/Mac/Users/me`) when
/// the library was made on another machine.
#[derive(Debug, Clone, Default)]
pub struct Remap(pub Vec<(PathBuf, PathBuf)>);

impl Remap {
    pub fn apply(&self, path: &Path) -> PathBuf {
        for (from, to) in &self.0 {
            if let Ok(rest) = path.strip_prefix(from) {
                return to.join(rest);
            }
        }
        path.to_path_buf()
    }
}

/// Works out what to put on the device. With `check_files`, tracks whose
/// file isn't there (after `remap`) are skipped.
pub fn plan<'a>(
    lib: &'a Library,
    selection: &'a Selection,
    conversion: Conversion,
    remap: &Remap,
    check_files: bool,
) -> Plan<'a> {
    let by_id: HashMap<u64, &Track> = lib.tracks.iter().map(|t| (t.persistent_id, t)).collect();
    let (playlists, missing_playlists) = if selection.everything {
        let all = lib.playlists.iter().filter(|p| p.is_user()).collect();
        (all, Vec::new())
    } else {
        selection.resolve_playlists(lib)
    };

    let mut wanted: Vec<&Track> = Vec::new();
    let mut seen = HashSet::new();
    let mut want = |t: &'a Track| {
        if seen.insert(t.persistent_id) {
            wanted.push(t);
        }
    };
    if selection.everything {
        lib.tracks.iter().for_each(&mut want);
    } else {
        for p in &playlists {
            p.track_ids
                .iter()
                .filter_map(|id| by_id.get(id))
                .for_each(|t| want(t));
        }
        for t in &lib.tracks {
            let artist =
                |a: &String| t.artist.as_ref() == Some(a) || t.album_artist.as_ref() == Some(a);
            if selection.artists.iter().any(artist)
                || selection.albums.iter().any(|a| a.matches(t))
                || selection.genres.iter().any(|g| t.genre.as_ref() == Some(g))
            {
                want(t);
            }
        }
    }
    wanted.sort_by_cached_key(|t| sort_key(t));

    let mut plan = Plan {
        missing_playlists,
        ..Plan::default()
    };
    for t in wanted {
        match decide(t, conversion, remap, check_files) {
            Ok((action, size)) => plan.items.push(Item {
                track: t,
                action,
                size,
            }),
            Err(skip) => plan.skipped.push((t, skip)),
        }
    }
    let planned: HashSet<u64> = plan.items.iter().map(|i| i.track.persistent_id).collect();
    plan.playlists = playlists
        .into_iter()
        .map(|p| {
            let ids = p
                .track_ids
                .iter()
                .copied()
                .filter(|id| planned.contains(id))
                .collect();
            (p, ids)
        })
        .collect();
    plan
}

/// What happens to one track and its size on the device, without checking
/// that its file exists.
pub fn track_outcome(t: &Track, conversion: Conversion) -> Result<(Action, u64), Skip> {
    decide(t, conversion, &Remap::default(), false)
}

fn decide(
    t: &Track,
    conversion: Conversion,
    remap: &Remap,
    check_files: bool,
) -> Result<(Action, u64), Skip> {
    let path = t.location.as_ref().ok_or(Skip::NoFile)?;
    let format = Format::of(t).unwrap_or(Format::Unplayable);
    match format {
        Format::Protected => return Err(Skip::Protected),
        Format::Video => return Err(Skip::Video),
        _ => {}
    }
    if check_files && !remap.apply(path).is_file() {
        return Err(Skip::Missing);
    }
    let action = match (format, conversion) {
        (Format::Unplayable, Conversion::Off) => Action::Convert {
            kbps: FALLBACK_KBPS,
        },
        (Format::Unplayable | Format::Lossless, Conversion::Aac { kbps }) => {
            Action::Convert { kbps }
        }
        (Format::Mp3 | Format::Aac, Conversion::Aac { kbps })
            if t.bit_rate.is_some_and(|r| r > kbps) =>
        {
            Action::Convert { kbps }
        }
        _ => Action::Copy,
    };
    let size = match action {
        Action::Copy => t.size.unwrap_or(0),
        Action::Convert { kbps } => estimate_aac_size(t, kbps),
    };
    Ok((action, size))
}

/// Encoded audio at `kbps`, plus 12%: Apple's constrained VBR averages
/// about 10% above the nominal rate (measured: 1078 MB for a 976 MB nominal
/// playlist), and the container adds a little.
pub fn estimate_aac_size(t: &Track, kbps: u32) -> u64 {
    match t.duration_ms {
        Some(ms) => u64::from(ms) * u64::from(kbps) / 8 * 112 / 100,
        None => t.size.unwrap_or(0),
    }
}

fn sort_key(t: &Track) -> (String, String, u16, u16, String) {
    let lower = |s: Option<&str>| s.unwrap_or_default().to_lowercase();
    (
        lower(album_artist(t)),
        lower(t.album.as_deref()),
        t.disc_number.unwrap_or(0),
        t.track_number.unwrap_or(0),
        lower(t.title.as_deref()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: u64, file: &str, kbps: u32) -> Track {
        Track {
            persistent_id: id,
            title: Some(format!("t{id}")),
            artist: Some("A".into()),
            album: Some("X".into()),
            genre: Some("Pop".into()),
            duration_ms: Some(200_000),
            size: Some(u64::from(kbps) * 200_000 / 8),
            bit_rate: Some(kbps),
            location: (!file.is_empty()).then(|| PathBuf::from(file)),
            ..Track::default()
        }
    }

    fn library() -> Library {
        Library {
            tracks: vec![
                track(1, "/m/a.mp3", 320),
                track(2, "/m/b.m4a", 256),
                track(3, "/m/c.m4a", 1000),
                track(4, "/m/d.flac", 900),
                track(5, "", 256),
                track(6, "/m/f.m4p", 256),
                Track {
                    artist: Some("B".into()),
                    genre: Some("Rock".into()),
                    ..track(7, "/m/g.mp3", 128)
                },
            ],
            playlists: vec![
                Playlist {
                    name: "Library".into(),
                    is_master: true,
                    track_ids: (1..=7).collect(),
                    ..Playlist::default()
                },
                Playlist {
                    persistent_id: Some(0xAB),
                    name: "mix".into(),
                    track_ids: vec![7, 2, 5],
                    ..Playlist::default()
                },
            ],
        }
    }

    fn ids(plan: &Plan) -> Vec<u64> {
        plan.items.iter().map(|i| i.track.persistent_id).collect()
    }

    #[test]
    fn selects_playlists_and_filters() {
        let lib = library();
        let mut sel = Selection {
            playlists: vec![PlaylistRef::new(&lib.playlists[1])],
            ..Selection::default()
        };
        let plan = plan(&lib, &sel, Conversion::Off, &Remap::default(), false);
        // Sorted by artist; the cloud-only track is skipped.
        assert_eq!(ids(&plan), [2, 7]);
        assert_eq!(plan.skipped, [(&lib.tracks[4], Skip::NoFile)]);
        assert_eq!(plan.playlists[0].1, [7, 2]);

        sel.playlists.clear();
        sel.genres.push("Rock".into());
        sel.artists.push("A".into());
        let plan = super::plan(&lib, &sel, Conversion::Off, &Remap::default(), false);
        assert_eq!(ids(&plan), [1, 2, 3, 4, 7]);
        assert!(plan.playlists.is_empty());
    }

    #[test]
    fn converts_like_itunes() {
        let lib = library();
        let sel = Selection {
            everything: true,
            ..Selection::default()
        };
        let action = |plan: &Plan, id| {
            plan.items
                .iter()
                .find(|i| i.track.persistent_id == id)
                .map(|i| i.action)
        };
        let off = plan(&lib, &sel, Conversion::Off, &Remap::default(), false);
        assert_eq!(action(&off, 1), Some(Action::Copy));
        assert_eq!(action(&off, 3), Some(Action::Copy));
        assert_eq!(action(&off, 4), Some(Action::Convert { kbps: 256 }));
        assert_eq!(action(&off, 6), None);

        let aac = plan(
            &lib,
            &sel,
            Conversion::Aac { kbps: 256 },
            &Remap::default(),
            false,
        );
        assert_eq!(action(&aac, 1), Some(Action::Convert { kbps: 256 }));
        assert_eq!(action(&aac, 2), Some(Action::Copy));
        assert_eq!(action(&aac, 3), Some(Action::Convert { kbps: 256 }));
        assert_eq!(action(&aac, 7), Some(Action::Copy));
        let converted = aac
            .items
            .iter()
            .find(|i| i.track.persistent_id == 1)
            .unwrap();
        assert_eq!(converted.size, 7_168_000);
    }

    #[test]
    fn remaps_paths() {
        let remap = Remap(vec![("/Users/me".into(), "/Volumes/Mac/Users/me".into())]);
        assert_eq!(
            remap.apply(Path::new("/Users/me/Music/a.mp3")),
            Path::new("/Volumes/Mac/Users/me/Music/a.mp3")
        );
        assert_eq!(remap.apply(Path::new("/other")), Path::new("/other"));
    }
}
