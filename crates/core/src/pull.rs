//! Bringing changes made on a device back into the library: songs added to
//! playlists, playlists made on the device, and play statistics.
//!
//! The iTunes library stays the master copy, so changes are applied with an
//! AppleScript run against iTunes. The script sets target values ("played
//! count to 12", "add this song unless it's there") rather than increments,
//! so running it twice is harmless. Statistics wait in a per-device pending
//! list until the library shows them.
//!
//! Playlists follow the rule `bt clones` uses: songs the device has and the
//! library playlist lacks are added; nothing is removed, because without a
//! record of the last sync a song missing on the device can't be told apart
//! from one added in iTunes since. Playlists the device got from the
//! library are left alone: the device can't edit them in place (it forks
//! them instead), so any difference there was made in iTunes since the
//! last sync, such as a song removed.

use crate::profile::data_dir;
use crate::{clones, Error, Library, Playlist, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// A playlist as the device has it.
#[derive(Debug, Clone, PartialEq)]
pub struct DevicePlaylist {
    /// The library playlist it was synced from, if any.
    pub sync_id: Option<u64>,
    pub name: String,
    /// Library persistent IDs, in order.
    pub tracks: Vec<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changes {
    pub additions: Vec<Addition>,
    pub new_playlists: Vec<(String, Vec<u64>)>,
    /// Copies iTunes made of playlists edited on a device ("Name 1"), to be
    /// deleted once their songs are in the original.
    pub copies: Vec<Copy>,
    pub stats: Vec<Stat>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Addition {
    pub playlist: u64,
    pub name: String,
    pub tracks: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Copy {
    pub copy: u64,
    pub name: String,
    pub original: u64,
}

/// Values a track should have in the library.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Stat {
    /// Persistent ID, in hex.
    pub track: String,
    pub play_count: u32,
    pub skip_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    /// Seconds since 1904, local time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<u32>,
}

impl Stat {
    fn id(&self) -> Option<u64> {
        u64::from_str_radix(&self.track, 16).ok()
    }
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.additions.is_empty()
            && self.new_playlists.is_empty()
            && self.copies.is_empty()
            && self.stats.is_empty()
    }
}

/// Playlist changes: what the device's playlists have beyond the library's,
/// playlists made on the device, and copies iTunes already made.
pub fn playlist_changes(lib: &Library, device: &[DevicePlaylist]) -> Changes {
    let known: HashSet<u64> = lib.tracks.iter().map(|t| t.persistent_id).collect();
    let editable = |p: &&Playlist| p.is_user() && !p.is_smart;
    let mut additions: Vec<Addition> = Vec::new();
    let mut add = |p: &Playlist, tracks: Vec<u64>| {
        let Some(id) = p.persistent_id else { return };
        let have: HashSet<u64> = p.track_ids.iter().copied().collect();
        let mut new: Vec<u64> = tracks
            .into_iter()
            .filter(|t| known.contains(t) && !have.contains(t))
            .collect();
        let mut seen = HashSet::new();
        new.retain(|t| seen.insert(*t));
        if new.is_empty() {
            return;
        }
        match additions.iter_mut().find(|a| a.playlist == id) {
            Some(a) => {
                for t in new {
                    if !a.tracks.contains(&t) {
                        a.tracks.push(t);
                    }
                }
            }
            None => additions.push(Addition {
                playlist: id,
                name: p.name.clone(),
                tracks: new,
            }),
        }
    };

    let mut new_playlists = Vec::new();
    for dp in device {
        if dp.sync_id.is_some() {
            continue;
        }
        // A playlist made on the device that shares a library playlist's
        // name is that playlist edited (the device forks playlists it
        // isn't allowed to change in place).
        match lib
            .playlists
            .iter()
            .filter(editable)
            .find(|p| p.name == dp.name)
        {
            Some(p) => add(p, dp.tracks.clone()),
            None => {
                let tracks: Vec<u64> = dp
                    .tracks
                    .iter()
                    .copied()
                    .filter(|t| known.contains(t))
                    .collect();
                if !tracks.is_empty() {
                    new_playlists.push((dp.name.clone(), tracks));
                }
            }
        }
    }

    let mut copies = Vec::new();
    for pair in clones::find(lib) {
        add(pair.original, pair.added.clone());
        if let (Some(copy), Some(original)) = (pair.copy.persistent_id, pair.original.persistent_id)
        {
            copies.push(Copy {
                copy,
                name: pair.copy.name.clone(),
                original,
            });
        }
    }
    Changes {
        additions,
        new_playlists,
        copies,
        stats: Vec::new(),
    }
}

/// Play statistics a device recorded since its last sync, per track.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Plays {
    pub plays: u32,
    pub skips: u32,
    /// Seconds since 1904.
    pub last_played: Option<u32>,
    /// The device's current rating, 1-100.
    pub rating: Option<u8>,
}

/// Statistics still to be applied to the library for one device.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pending {
    pub stats: Vec<Stat>,
}

impl Pending {
    fn path(device: &str) -> Result<PathBuf> {
        if device.is_empty() || device.contains(['/', '\\']) {
            return Err(Error::format("bad device id"));
        }
        Ok(data_dir()?.join("pending").join(format!("{device}.plist")))
    }

    pub fn load(device: &str) -> Result<Pending> {
        match std::fs::read(Pending::path(device)?) {
            Ok(b) => {
                plist::from_bytes(&b).map_err(|e| Error::format(format!("pending changes: {e}")))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Pending::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, device: &str) -> Result<()> {
        let path = Pending::path(device)?;
        if self.stats.is_empty() {
            let _ = std::fs::remove_file(path);
            return Ok(());
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("tmp");
        plist::to_file_xml(&tmp, self).map_err(|e| Error::format(e.to_string()))?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// Adds plays recorded on the device on top of what's already pending
    /// (or, for tracks with nothing pending, the library's values).
    pub fn record(&mut self, lib: &Library, plays: &HashMap<u64, Plays>) {
        for (&id, p) in plays {
            if p.plays == 0 && p.skips == 0 && p.rating.is_none() {
                continue;
            }
            let Some(t) = lib.track(id) else { continue };
            let i = match self.stats.iter().position(|s| s.id() == Some(id)) {
                Some(i) => i,
                None => {
                    self.stats.push(Stat {
                        track: format!("{id:016X}"),
                        play_count: t.play_count,
                        skip_count: t.skip_count,
                        rating: None,
                        last_played: t.last_played,
                    });
                    self.stats.len() - 1
                }
            };
            let s = &mut self.stats[i];
            s.play_count += p.plays;
            s.skip_count += p.skips;
            s.last_played = s.last_played.max(p.last_played);
            if p.rating.is_some() && p.rating != t.rating {
                s.rating = p.rating;
            }
        }
        self.stats.sort_by(|a, b| a.track.cmp(&b.track));
    }

    /// Drops what the library already shows.
    pub fn prune(&mut self, lib: &Library) {
        self.stats.retain(|s| {
            let Some(t) = s.id().and_then(|id| lib.track(id)) else {
                return false;
            };
            t.play_count < s.play_count
                || t.skip_count < s.skip_count
                || (s.rating.is_some() && s.rating != t.rating)
                || s.last_played > t.last_played
        });
    }
}

/// An AppleScript that applies `changes` to iTunes.
pub fn script(changes: &Changes) -> String {
    let mut s = String::from(
        "-- Generated by betterTunes (bt pull): changes made on a device.\n\
         -- Run on the Mac with the iTunes library. Safe to run more than once.\n\n\
         on addTo(p, tid)\n\
         \ttell application \"iTunes\"\n\
         \t\tif (count (tracks of p whose persistent ID is tid)) = 0 then\n\
         \t\t\tduplicate (first track of library playlist 1 whose persistent ID is tid) to p\n\
         \t\tend if\n\
         \tend tell\n\
         end addTo\n\n\
         on mkDate(y, m, d, secs)\n\
         \tset t to current date\n\
         \tset day of t to 1\n\
         \tset year of t to y\n\
         \tset month of t to m\n\
         \tset day of t to d\n\
         \tset time of t to secs\n\
         \treturn t\n\
         end mkDate\n\n\
         set failures to {}\n",
    );
    let pid = |id: u64| format!("\"{id:016X}\"");
    for a in &changes.additions {
        s += &format!(
            "-- {}: add {} songs\ntry\n",
            one_line(&a.name),
            a.tracks.len()
        );
        s += &format!(
            "\ttell application \"iTunes\" to set p to (first user playlist whose persistent ID is {})\n",
            pid(a.playlist)
        );
        for &t in &a.tracks {
            s += &format!("\tmy addTo(p, {})\n", pid(t));
        }
        s += &on_error(&a.name);
    }
    for (name, tracks) in &changes.new_playlists {
        s += &format!("-- new playlist {}\ntry\n", one_line(name));
        s += &format!(
            "\ttell application \"iTunes\"\n\
             \t\tif (count (user playlists whose name is {q})) = 0 then\n\
             \t\t\tset p to make new user playlist with properties {{name:{q}}}\n\
             \t\telse\n\
             \t\t\tset p to first user playlist whose name is {q}\n\
             \t\tend if\n\
             \tend tell\n",
            q = quote(name)
        );
        for &t in tracks {
            s += &format!("\tmy addTo(p, {})\n", pid(t));
        }
        s += &on_error(name);
    }
    for c in &changes.copies {
        s += &format!(
            "-- delete the copy {} once all its songs are in the original\n\
             try\n\
             \ttell application \"iTunes\"\n\
             \t\tset c to (first user playlist whose persistent ID is {})\n\
             \t\tset o to (first user playlist whose persistent ID is {})\n\
             \t\tset complete to true\n\
             \t\trepeat with t in (tracks of c)\n\
             \t\t\tset tid to persistent ID of t\n\
             \t\t\tif (count (tracks of o whose persistent ID is tid)) = 0 then set complete to false\n\
             \t\tend repeat\n\
             \t\tif complete then\n\
             \t\t\tdelete c\n\
             \t\telse\n\
             \t\t\tset end of failures to {} & \": songs missing from the original, copy kept\"\n\
             \t\tend if\n\
             \tend tell\n",
            one_line(&c.name),
            pid(c.copy),
            pid(c.original),
            quote(&c.name)
        );
        s += &on_error(&c.name);
    }
    for st in &changes.stats {
        s += &format!(
            "try\n\
             \ttell application \"iTunes\"\n\
             \t\tset t to (first track of library playlist 1 whose persistent ID is \"{}\")\n\
             \t\tif played count of t < {pc} then set played count of t to {pc}\n\
             \t\tif skipped count of t < {sc} then set skipped count of t to {sc}\n",
            st.track,
            pc = st.play_count,
            sc = st.skip_count
        );
        if let Some(r) = st.rating {
            s += &format!("\t\tset rating of t to {r}\n");
        }
        if let Some((y, m, d, secs)) = st.last_played.map(civil_from_mac) {
            s += &format!(
                "\t\tset when to my mkDate({y}, {m}, {d}, {secs})\n\
                 \t\tif played date of t is missing value or played date of t < when then set played date of t to when\n"
            );
        }
        s += "\tend tell\n";
        s += &on_error(&st.track);
    }
    s += "\nif failures is {} then\n\
          \tdisplay dialog \"betterTunes: device changes applied.\" buttons {\"OK\"} default button 1\n\
          else\n\
          \tset AppleScript's text item delimiters to return\n\
          \tdisplay dialog \"betterTunes: some changes weren't applied:\" & return & return & (failures as text) buttons {\"OK\"} default button 1\n\
          end if\n";
    s
}

fn on_error(label: &str) -> String {
    format!(
        "on error errMsg\n\tset end of failures to {} & \": \" & errMsg\nend try\n\n",
        quote(label)
    )
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

/// An AppleScript string literal.
fn quote(s: &str) -> String {
    format!(
        "\"{}\"",
        one_line(s).replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// Year, month, day and seconds into the day of a time in seconds since
/// 1904.
fn civil_from_mac(mac: u32) -> (i64, u32, u32, u32) {
    let unix = i64::from(mac) - 2_082_844_800;
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400) as u32);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day, secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Track;

    fn lib() -> Library {
        let track = |id| Track {
            persistent_id: id,
            play_count: 3,
            ..Track::default()
        };
        let playlist = |id, name: &str, tracks: &[u64]| Playlist {
            persistent_id: Some(id),
            name: name.into(),
            track_ids: tracks.to_vec(),
            ..Playlist::default()
        };
        Library {
            tracks: (1..=6).map(track).collect(),
            playlists: vec![
                playlist(0xA, "Favourites", &[1, 2]),
                playlist(0xB, "road trip", &[3]),
                playlist(0xC, "road trip 1", &[3, 4]),
            ],
        }
    }

    #[test]
    fn merges_device_edits_and_copies() {
        let lib = lib();
        let device = [
            // Synced from the library: differences were made in iTunes.
            DevicePlaylist {
                sync_id: Some(0xA),
                name: "Favourites".into(),
                tracks: vec![2, 5, 1],
            },
            // Forked: made on the device under the library playlist's name.
            DevicePlaylist {
                sync_id: None,
                name: "Favourites".into(),
                tracks: vec![1, 6, 99],
            },
            DevicePlaylist {
                sync_id: None,
                name: "new one".into(),
                tracks: vec![4, 99],
            },
            DevicePlaylist {
                sync_id: None,
                name: "empty".into(),
                tracks: vec![99],
            },
        ];
        let c = playlist_changes(&lib, &device);
        assert_eq!(
            c.additions,
            [
                Addition {
                    playlist: 0xA,
                    name: "Favourites".into(),
                    tracks: vec![6]
                },
                Addition {
                    playlist: 0xB,
                    name: "road trip".into(),
                    tracks: vec![4]
                },
            ]
        );
        assert_eq!(c.new_playlists, [("new one".to_string(), vec![4])]);
        assert_eq!(
            c.copies,
            [Copy {
                copy: 0xC,
                name: "road trip 1".into(),
                original: 0xB
            }]
        );
        let script = script(&c);
        assert!(script.contains("my addTo(p, \"0000000000000006\")"));
        assert!(script.contains("delete c"));
    }

    #[test]
    fn tracks_pending_plays_until_the_library_has_them() {
        let mut lib = lib();
        let mut pending = Pending::default();
        let plays = HashMap::from([(
            2,
            Plays {
                plays: 2,
                skips: 1,
                last_played: Some(3_900_000_000),
                rating: Some(80),
            },
        )]);
        pending.record(&lib, &plays);
        pending.record(
            &lib,
            &HashMap::from([(
                2,
                Plays {
                    plays: 1,
                    ..Plays::default()
                },
            )]),
        );
        assert_eq!(pending.stats[0].play_count, 3 + 2 + 1);
        assert_eq!(pending.stats[0].rating, Some(80));
        pending.prune(&lib);
        assert_eq!(pending.stats.len(), 1);
        let t = lib
            .tracks
            .iter_mut()
            .find(|t| t.persistent_id == 2)
            .unwrap();
        t.play_count = 6;
        t.skip_count = 1;
        t.rating = Some(80);
        t.last_played = Some(3_900_000_000);
        pending.prune(&lib);
        assert!(pending.stats.is_empty());
    }

    #[test]
    fn converts_mac_dates() {
        // 2026-11-14 12:00 is Unix time 1_794_657_600.
        assert_eq!(
            civil_from_mac(1_794_657_600 + 2_082_844_800),
            (2026, 11, 14, 43_200)
        );
    }
}
