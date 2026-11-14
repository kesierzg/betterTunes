//! `bt pull`: bring changes made on a device back into the iTunes library.
//!
//! - iPod touch 1G (iPhone OS 1.x): play counts, skips, ratings and last
//!   played dates from its "Play Counts" file. These wait in betterTunes'
//!   pending list, and the file is removed from the device so they're never
//!   counted twice.
//! - iPhones (iOS 5 and later): playlist edits, from the music database
//!   (`MediaLibrary.sqlitedb`). Editing a playlist synced from iTunes makes
//!   the phone fork it into a new playlist, which iTunes then imports as
//!   "Name 1"; both the fork and such copies are merged into the original.
//!   Play statistics are left to iTunes, which still syncs these phones and
//!   would otherwise count them twice.
//!
//! The changes are applied with an AppleScript: run directly where iTunes is
//! installed, otherwise saved for running on the Mac that has it.

use crate::sync::track_name;
use bettertunes_core::itunesdb;
use bettertunes_core::profile::{self, Profile};
use bettertunes_core::pull::{self, Changes, DevicePlaylist, Pending, Plays};
use bettertunes_core::Library;
use bettertunes_device::afc::{self, Afc};
use bettertunes_device::Device;
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const PLAY_COUNTS: &str = "/iTunes_Control/iTunes/Play Counts";
const ITUNES: &str = "/Applications/iTunes.app";

/// What a device has that the library doesn't, gathered and saved.
pub struct Pulled {
    pub changes: Changes,
    pub plays_recorded: usize,
}

/// Reads the device's changes. Play statistics go into the pending list
/// (and are removed from the device); the returned changes include
/// everything still pending.
pub fn gather(device: &Device, profile: &Profile, lib: &Library) -> Result<Pulled, String> {
    let fail = |e: bettertunes_device::Error| e.to_string();
    let version = device
        .session()
        .and_then(|mut s| s.get_value("ProductVersion"))
        .map_err(fail)?
        .and_then(|v| v.as_string().map(str::to_owned))
        .unwrap_or_default();
    let mut afc = Afc::new(device.start_service(afc::SERVICE).map_err(fail)?);
    let mut pending = Pending::load(&profile.id).map_err(|e| e.to_string())?;
    let mut device_playlists = Vec::new();
    let mut plays_recorded = 0;
    if version.starts_with("1.") {
        plays_recorded = save_touch_plays(&mut afc, &profile.id, lib)?;
        pending = Pending::load(&profile.id).map_err(|e| e.to_string())?;
    } else {
        device_playlists = iphone_playlists(&mut afc)?;
    }
    pending.prune(lib);
    pending.save(&profile.id).map_err(|e| e.to_string())?;
    let mut changes = pull::playlist_changes(lib, &device_playlists);
    changes.stats = pending.stats;
    Ok(Pulled {
        changes,
        plays_recorded,
    })
}

/// Moves the touch's Play Counts into the pending list and removes the
/// file, so the counts start again from zero. Returns how many songs had
/// something recorded. Must run before a sync replaces the iTunesDB, which
/// the file's entries refer to by position.
pub fn save_touch_plays(afc: &mut Afc, device_id: &str, lib: &Library) -> Result<usize, String> {
    let plays = touch_plays(afc).map_err(|e| e.to_string())?;
    if plays.is_empty() {
        return Ok(0);
    }
    let mut pending = Pending::load(device_id).map_err(|e| e.to_string())?;
    pending.record(lib, &plays);
    pending.save(device_id).map_err(|e| e.to_string())?;
    afc.remove(PLAY_COUNTS).map_err(|e| e.to_string())?;
    Ok(plays
        .values()
        .filter(|p| p.plays + p.skips > 0 || p.rating.is_some())
        .count())
}

/// The touch's Play Counts, by track dbid (the library persistent ID).
fn touch_plays(afc: &mut Afc) -> bettertunes_device::Result<HashMap<u64, Plays>> {
    if afc.file_info(PLAY_COUNTS)?.is_none() {
        return Ok(HashMap::new());
    }
    let counts = afc.read_file(PLAY_COUNTS)?;
    let db = afc.read_file("/iTunes_Control/iTunes/iTunesDB")?;
    let bad = |e: bettertunes_core::Error| bettertunes_device::Error::Protocol(e.to_string());
    let tracks = itunesdb::parse(&db).map_err(bad)?.tracks;
    let counts = itunesdb::parse_play_counts(&counts).map_err(bad)?;
    Ok(tracks
        .iter()
        .zip(counts)
        .map(|(t, p)| (t.dbid, p))
        .collect())
}

/// The phone's own playlists, read from a copy of its music database.
fn iphone_playlists(afc: &mut Afc) -> Result<Vec<DevicePlaylist>, String> {
    let dir = std::env::temp_dir().join(format!("bettertunes-pull-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let result = (|| {
        for name in [
            "MediaLibrary.sqlitedb",
            "MediaLibrary.sqlitedb-wal",
            "MediaLibrary.sqlitedb-shm",
        ] {
            let path = format!("/iTunes_Control/iTunes/{name}");
            if afc.file_info(&path).map_err(|e| e.to_string())?.is_some() {
                let data = afc.read_file(&path).map_err(|e| e.to_string())?;
                std::fs::write(dir.join(name), data).map_err(|e| e.to_string())?;
            }
        }
        read_playlists(&dir.join("MediaLibrary.sqlitedb")).map_err(|e| e.to_string())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Ordinary playlists (not smart, not folders, not built-in) with their
/// songs as library persistent IDs. Songs that didn't come from the library
/// are left out.
fn read_playlists(db: &Path) -> rusqlite::Result<Vec<DevicePlaylist>> {
    let conn = rusqlite::Connection::open(db)?;
    let mut containers = conn.prepare(
        "SELECT container_pid, name, sync_id, is_src_remote FROM container
         WHERE distinguished_kind = 0 AND is_hidden = 0 AND smart_is_folder = 0
           AND smart_criteria IS NULL",
    )?;
    let mut items = conn.prepare(
        "SELECT s.sync_id FROM container_item ci JOIN item_store s ON s.item_pid = ci.item_pid
         WHERE ci.container_pid = ?1 AND s.sync_id != 0 ORDER BY ci.position",
    )?;
    let rows: Vec<(i64, String, i64, i64)> = containers
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(pid, name, sync_id, remote)| {
            let tracks = items
                .query_map([pid], |r| r.get::<_, i64>(0))?
                .map(|id| id.map(|id| id as u64))
                .collect::<rusqlite::Result<_>>()?;
            Ok(DevicePlaylist {
                sync_id: (remote != 0 && sync_id != 0).then_some(sync_id as u64),
                name,
                tracks,
            })
        })
        .collect()
}

/// Lists the changes, then applies them: directly where iTunes is
/// installed, otherwise by saving the script.
pub fn run(
    device: &Device,
    profile: &Profile,
    lib: &Library,
    script_path: Option<PathBuf>,
    yes: bool,
    out: &mut impl Write,
) -> io::Result<ExitCode> {
    let pulled = match gather(device, profile, lib) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bt: pull failed: {e}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let c = &pulled.changes;
    if pulled.plays_recorded > 0 {
        writeln!(
            out,
            "Saved play counts for {} songs from {}.",
            pulled.plays_recorded, profile.name
        )?;
    }
    if c.is_empty() {
        writeln!(out, "Nothing to bring back from {}.", profile.name)?;
        return Ok(ExitCode::SUCCESS);
    }
    describe(lib, c, out)?;
    if itunes_here() && script_path.is_none() && !yes && !confirm()? {
        writeln!(
            out,
            "Not applied. (Play counts stay pending for next time.)"
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    match apply(c, profile, script_path) {
        Ok(msg) => {
            writeln!(out, "{msg}")?;
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("bt: {e}");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn itunes_here() -> bool {
    cfg!(target_os = "macos") && Path::new(ITUNES).exists()
}

/// Runs the script in iTunes if it's on this Mac (and no file was asked
/// for), otherwise saves it. Returns what happened.
pub fn apply(
    c: &Changes,
    profile: &Profile,
    script_path: Option<PathBuf>,
) -> Result<String, String> {
    let script = pull::script(c);
    if itunes_here() && script_path.is_none() {
        let mut child = Command::new("osascript")
            .arg("-")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .map_err(|e| e.to_string())?;
        let status = child.wait().map_err(|e| e.to_string())?;
        return if status.success() {
            Ok("Applied in iTunes.".into())
        } else {
            Err("iTunes reported a problem; see its dialog".into())
        };
    }
    let path = match script_path {
        Some(p) => p,
        None => {
            let dir = profile::data_dir()
                .map_err(|e| e.to_string())?
                .join("scripts");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            dir.join(format!("pull-{}-{stamp}.applescript", profile.id))
        }
    };
    std::fs::write(&path, script).map_err(|e| e.to_string())?;
    Ok(format!(
        "iTunes isn't on this Mac, so the changes were saved as a script:\n    {}\n\
         Run it on the Mac with your iTunes library (double-click, or `osascript FILE`). \
         It's safe to run more than once.",
        path.display()
    ))
}

fn describe(lib: &Library, c: &Changes, out: &mut impl Write) -> io::Result<()> {
    let name = |id: &u64| {
        lib.track(*id)
            .map_or_else(|| format!("{id:016X}"), track_name)
    };
    for a in &c.additions {
        writeln!(out, "Add to \"{}\":", a.name)?;
        for t in &a.tracks {
            writeln!(out, "    + {}", name(t))?;
        }
    }
    for (playlist, tracks) in &c.new_playlists {
        writeln!(out, "New playlist \"{playlist}\" ({} songs)", tracks.len())?;
    }
    for copy in &c.copies {
        writeln!(
            out,
            "Delete the copy \"{}\" once its songs are in the original",
            copy.name
        )?;
    }
    if !c.stats.is_empty() {
        let plays: u32 = c
            .stats
            .iter()
            .filter_map(|s| {
                let id = u64::from_str_radix(&s.track, 16).ok()?;
                Some(s.play_count.saturating_sub(lib.track(id)?.play_count))
            })
            .sum();
        writeln!(
            out,
            "Update play counts and ratings of {} songs ({plays} plays)",
            c.stats.len()
        )?;
    }
    Ok(())
}

fn confirm() -> io::Result<bool> {
    if !io::stdin().is_terminal() {
        eprintln!("bt: no terminal to ask in; pass --yes to apply");
        return Ok(false);
    }
    dialoguer::Confirm::new()
        .with_prompt("Apply these changes in iTunes?")
        .default(true)
        .interact()
        .map_err(|dialoguer::Error::IO(e)| e)
}
