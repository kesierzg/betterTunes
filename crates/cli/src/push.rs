//! Syncing a plan onto an iPhone OS 1.x device (the iPod touch 1G): copy the
//! songs into `iTunes_Control/Music`, then replace the iTunesDB with a new,
//! signed one. Later iOS versions keep their library elsewhere and need the
//! on-device helper instead.
//!
//! The sync is bracketed the way iTunes (and libgpod) do it: post
//! "syncWillStart", take the lock on `/com.apple.itunes.lock_sync` after
//! asking for it with "syncLockRequest", post "syncDidStart"; at the end
//! release the lock and post "syncDidFinish", which makes the device reload
//! its library.
//!
//! Songs are stored as `F<nn>/<persistent ID>.<ext>`, so a later sync can
//! keep files that are already there with the right size.

use bettertunes_core::artwork::{self, Format};
use bettertunes_core::itunesdb::{Database, DbPlaylist, DbTrack};
use bettertunes_core::profile::{self, Profile};
use bettertunes_core::sync::{self, Action, Remap, Skip};
use bettertunes_core::transcode::{self, Encoder};
use bettertunes_core::{hash58, Library, Track};
use bettertunes_device::afc::{self, Afc};
use bettertunes_device::{notify, Device};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MUSIC: &str = "/iTunes_Control/Music";
const DATABASE: &str = "/iTunes_Control/iTunes/iTunesDB";
const ARTWORK: &str = "/iTunes_Control/Artwork";
const FOLDERS: u64 = 50;

/// Finds the connected device with `udid`. A device that was just used may
/// take a moment to be free again, so this retries briefly.
pub fn find(udid: &str) -> Option<Device> {
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(700));
        }
        let found = bettertunes_device::find_devices(|_| {})
            .into_iter()
            .find(|d| d.udid == udid && d.connection() != "Network");
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Progress reports.
pub enum Event<'a> {
    Step(&'a str),
    Converting {
        done: usize,
        total: usize,
    },
    Copying {
        done: usize,
        total: usize,
        track: &'a Track,
    },
}

#[derive(Debug, Default)]
pub struct Report {
    pub copied: usize,
    pub kept: usize,
    pub removed: usize,
    pub bytes: u64,
    pub songs: usize,
    pub playlists: usize,
    /// Songs that couldn't be converted or copied, with why.
    pub failed: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn run(
    device: &Device,
    lib: &Library,
    profile: &Profile,
    remap: &Remap,
    progress: &(dyn Fn(Event) + Sync),
) -> Result<Report, String> {
    let fail = |e: bettertunes_device::Error| e.to_string();
    let (version, art_formats) = {
        let mut session = device.session().map_err(fail)?;
        let version = session
            .get_value("ProductVersion")
            .map_err(fail)?
            .and_then(|v| v.as_string().map(str::to_owned))
            .unwrap_or_default();
        let art = session
            .get_domain_value(Some("com.apple.mobile.iTunes"), Some("AlbumArt"))
            .ok()
            .flatten();
        (version, art.as_ref().map(art_formats).unwrap_or_default())
    };
    if !version.starts_with("1.") {
        return Err(format!(
            "syncing iOS {version} needs betterTunes' on-device helper, which isn't built yet; \
             only iPhone OS 1.x (the first iPod touch) can be synced so far"
        ));
    }
    let guid = firewire_guid(&device.udid).ok_or("device UDID isn't 40 hex digits")?;

    progress(Event::Step("Checking song files"));
    let plan = sync::plan(lib, &profile.selection, profile.conversion, remap, true);
    if plan.items.is_empty() {
        return Err("nothing chosen to sync".into());
    }
    let mut report = Report::default();
    for (t, skip) in &plan.skipped {
        if *skip == Skip::Missing {
            report.failed.push(format!("{}: file not found", name(t)));
        }
    }

    // Convert first, so the copy can't stall on the encoder.
    let cache = profile::cache_dir().map_err(|e| e.to_string())?;
    let (todo, _) = transcode::jobs(&plan, &cache, remap);
    let mut not_converted = HashSet::new();
    if !todo.is_empty() {
        let encoders = Encoder::available();
        if encoders.is_empty() {
            return Err("no AAC encoder found; install ffmpeg".into());
        }
        let failures = Mutex::new(Vec::new());
        transcode::run(
            &todo,
            &encoders,
            transcode::default_threads(),
            |done, job, result| {
                if let Err(e) = result {
                    failures.lock().unwrap().push((job.track.persistent_id, e));
                }
                progress(Event::Converting {
                    done,
                    total: todo.len(),
                });
                true
            },
        );
        for (id, e) in failures.into_inner().unwrap() {
            not_converted.insert(id);
            report.failed.push(e);
        }
    }

    // What goes where.
    struct Copy<'a> {
        track: &'a Track,
        source: PathBuf,
        dest: String,
        size: u64,
        kbps: Option<u32>,
    }
    let mut copies = Vec::new();
    for item in &plan.items {
        let t = item.track;
        if not_converted.contains(&t.persistent_id) {
            continue;
        }
        let (source, kbps) = match item.action {
            Action::Convert { kbps } => (transcode::cached_path(&cache, t, kbps), Some(kbps)),
            Action::Copy => (remap.apply(t.location.as_ref().unwrap()), None),
        };
        let Ok(meta) = std::fs::metadata(&source) else {
            report.failed.push(format!("{}: file not found", name(t)));
            continue;
        };
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("m4a")
            .to_ascii_lowercase();
        let dest = format!(
            "{MUSIC}/F{:02}/{:016X}.{ext}",
            t.persistent_id % FOLDERS,
            t.persistent_id
        );
        copies.push(Copy {
            track: t,
            source,
            dest,
            size: meta.len(),
            kbps,
        });
    }

    let mut afc = Afc::new(device.start_service(afc::SERVICE).map_err(fail)?);
    let info = afc.device_info().map_err(fail)?;
    let free: u64 = info
        .get("FSFreeBytes")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    progress(Event::Step("Starting the sync on the device"));
    let lock = begin(device, &mut afc).map_err(fail)?;
    // Play counts refer to the old database; keep them before replacing it.
    let saved = crate::pull::save_touch_plays(&mut afc, &profile.id, lib);
    if let Err(e) = saved {
        let _ = end(device, &mut afc, lock);
        return Err(format!("couldn't save the device's play counts: {e}"));
    }
    let result = (|| {
        let existing: HashMap<String, u64> = afc
            .walk(MUSIC)
            .map_err(fail)?
            .into_iter()
            .filter_map(|e| Some((e.path, e.size?)))
            .collect();
        let wanted: HashSet<&str> = copies.iter().map(|c| c.dest.as_str()).collect();
        let reclaimable: u64 = existing
            .iter()
            .filter(|(p, _)| !wanted.contains(p.as_str()))
            .map(|(_, s)| s)
            .sum();
        let to_copy: u64 = copies
            .iter()
            .filter(|c| existing.get(&c.dest) != Some(&c.size))
            .map(|c| c.size)
            .sum();
        // Keep 200 MB free, as the fit check does.
        if to_copy + (200 << 20) > free + reclaimable {
            return Err(format!(
                "doesn't fit: {} MB to copy, {} MB free",
                to_copy >> 20,
                (free + reclaimable) >> 20
            ));
        }

        // Remove what's no longer wanted first, to make room.
        progress(Event::Step("Removing songs no longer chosen"));
        for path in existing.keys().filter(|p| !wanted.contains(p.as_str())) {
            afc.remove(path).map_err(fail)?;
            report.removed += 1;
        }
        let dirs: HashSet<String> = afc
            .read_dir(MUSIC)
            .unwrap_or_default()
            .into_iter()
            .collect();
        for n in 0..FOLDERS {
            let dir = format!("F{n:02}");
            if !dirs.contains(&dir) {
                afc.make_dir(&format!("{MUSIC}/{dir}")).map_err(fail)?;
            }
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut tracks = Vec::new();
        // Original files, for their cover art (converted copies may lack it).
        let mut originals = Vec::new();
        let mut db_ids = HashMap::new();
        for (i, c) in copies.iter().enumerate() {
            progress(Event::Copying {
                done: i,
                total: copies.len(),
                track: c.track,
            });
            if existing.get(&c.dest) == Some(&c.size) {
                report.kept += 1;
            } else {
                let copied = std::fs::File::open(&c.source)
                    .map_err(|e| e.to_string())
                    .and_then(|f| {
                        afc.write_file(&c.dest, std::io::BufReader::new(f))
                            .map_err(fail)
                    });
                match copied {
                    Ok(n) => {
                        report.copied += 1;
                        report.bytes += n;
                    }
                    Err(e) => {
                        report.failed.push(format!("{}: {e}", name(c.track)));
                        let _ = afc.remove(&c.dest);
                        continue;
                    }
                }
            }
            let id = 100 + tracks.len() as u32;
            db_ids.insert(c.track.persistent_id, id);
            tracks.push(db_track(c.track, id, &c.dest, c.size, c.kbps, now));
            originals.push(c.track.location.as_ref().map(|l| remap.apply(l)));
        }

        if !art_formats.is_empty() {
            progress(Event::Step("Preparing cover art"));
            let (db, files) = cover_art(&art_formats, &mut tracks, &originals);
            progress(Event::Step("Copying cover art"));
            write_artwork(&mut afc, &db, &files).map_err(fail)?;
        }

        progress(Event::Step("Writing the music database"));
        let playlists = plan
            .playlists
            .iter()
            .map(|(p, ids)| DbPlaylist {
                id: p.persistent_id.unwrap_or_else(|| name_id(&p.name)),
                name: p.name.clone(),
                tracks: ids
                    .iter()
                    .filter_map(|id| db_ids.get(id).copied())
                    .collect(),
            })
            .collect::<Vec<_>>();
        report.songs = tracks.len();
        report.playlists = playlists.len();
        let db = Database {
            id: name_id(&device.udid) ^ now,
            name: profile.name.clone(),
            tracks,
            playlists,
        };
        let mut bytes = db.to_bytes();
        hash58::sign(&mut bytes, guid);
        let tmp = format!("{DATABASE}.bettertunes");
        afc.write_file(&tmp, &bytes[..]).map_err(fail)?;
        afc.rename(&tmp, DATABASE).map_err(fail)?;
        Ok(())
    })();
    progress(Event::Step("Finishing"));
    let finished = end(device, &mut afc, lock).map_err(fail);
    result?;
    finished?;
    Ok(report)
}

/// The cover art formats a device lists under "AlbumArt": format IDs, each
/// followed by its description. Only 16-bit RGB555 ("L555") is supported,
/// and formats tied to another one (video, chapters) are left out, as iTunes
/// does.
fn art_formats(value: &plist::Value) -> Vec<Format> {
    let Some(list) = value.as_array() else {
        return Vec::new();
    };
    let int = |d: &plist::Dictionary, k: &str| {
        d.get(k)
            .and_then(plist::Value::as_unsigned_integer)
            .unwrap_or(0) as u32
    };
    list.iter()
        .filter_map(plist::Value::as_dictionary)
        .filter(|d| d.get("PixelFormat").and_then(plist::Value::as_string) == Some("4C353535"))
        .filter(|d| !d.contains_key("AssociatedFormat"))
        .map(|d| {
            let color = d
                .get("BackColor")
                .and_then(plist::Value::as_string)
                .unwrap_or("00000000");
            let color = u32::from_str_radix(color, 16).unwrap_or(0).to_be_bytes();
            Format {
                id: int(d, "FormatId"),
                width: int(d, "RenderWidth"),
                height: int(d, "RenderHeight"),
                inset: int(d, "TopInset"),
                twiddled: int(d, "PixelOrder") == 1,
                background: color,
                row_alignment: int(d, "RowBytesAlignment").max(1),
                offset_alignment: int(d, "OffsetAlignment").max(1),
            }
        })
        .filter(|f| f.width > 2 * f.inset && f.height > 2 * f.inset)
        .collect()
}

/// Takes each track's cover from its original file, renders every distinct
/// cover once in each format, links the tracks to them, and returns the
/// ArtworkDB and ithmb files.
fn cover_art(
    formats: &[Format],
    tracks: &mut [DbTrack],
    originals: &[Option<PathBuf>],
) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let covers = par_map(originals, |p| p.as_deref().and_then(artwork::extract));
    // One image per distinct cover.
    let mut distinct: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut by_hash = HashMap::new();
    let mut image_of = vec![None; tracks.len()];
    for (i, cover) in covers.into_iter().enumerate() {
        let Some(cover) = cover else { continue };
        let mut h = DefaultHasher::new();
        cover.hash(&mut h);
        let index = *by_hash.entry(h.finish()).or_insert_with(|| {
            distinct.push((cover, tracks[i].dbid));
            distinct.len() - 1
        });
        image_of[i] = Some(index);
    }
    let rendered = par_map(&distinct, |(bytes, _)| {
        let image = image::load_from_memory(bytes).ok()?;
        Some(formats.iter().map(|f| f.render(&image)).collect::<Vec<_>>())
    });
    // Number the images that decoded.
    let mut images = Vec::new();
    let mut pixels = Vec::new();
    let mut id_of = vec![None; distinct.len()];
    for (index, (r, (bytes, song))) in rendered.into_iter().zip(&distinct).enumerate() {
        if let Some(r) = r {
            let id = 100 + images.len() as u32;
            id_of[index] = Some((id, bytes.len() as u32));
            images.push(artwork::Image {
                id,
                song: *song,
                source_size: bytes.len() as u32,
            });
            pixels.push(r);
        }
    }
    for (t, image) in tracks.iter_mut().zip(image_of) {
        t.artwork = image.and_then(|i| id_of[i]);
    }
    artwork::build(formats, &images, &pixels)
}

fn write_artwork(
    afc: &mut Afc,
    db: &[u8],
    files: &[(String, Vec<u8>)],
) -> bettertunes_device::Result<()> {
    if afc.file_info(ARTWORK)?.is_none() {
        afc.make_dir(ARTWORK)?;
    }
    for (name, data) in files {
        afc.write_file(&format!("{ARTWORK}/{name}"), &data[..])?;
    }
    let keep: HashSet<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    for name in afc.read_dir(ARTWORK)? {
        if name.ends_with(".ithmb") && !keep.contains(name.as_str()) {
            afc.remove(&format!("{ARTWORK}/{name}"))?;
        }
    }
    let tmp = format!("{ARTWORK}/ArtworkDB.bettertunes");
    afc.write_file(&tmp, db)?;
    afc.rename(&tmp, &format!("{ARTWORK}/ArtworkDB"))
}

/// Maps `f` over `items` on all cores, keeping the order.
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results = Mutex::new((0..items.len()).map(|_| None).collect::<Vec<Option<R>>>());
    std::thread::scope(|s| {
        for _ in 0..transcode::default_threads().min(items.len().max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else { break };
                let r = f(item);
                results.lock().unwrap()[i] = Some(r);
            });
        }
    });
    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(Option::unwrap)
        .collect()
}

/// Announces the sync. iPhone OS 1.x has no sync lock to take: its
/// SpringBoard answers "syncWillStart" with "syncingUnblocked", while
/// `/com.apple.itunes.lock_sync` stays locked and "syncLockRequest" goes
/// unanswered (seen in the device log). Later versions hand the lock over
/// on request; that's for when those are synced this way.
fn begin(device: &Device, _afc: &mut Afc) -> bettertunes_device::Result<Option<u64>> {
    notify::post(device, notify::SYNC_WILL_START)?;
    // Give the music player a moment to stop using the library.
    std::thread::sleep(Duration::from_secs(1));
    notify::post(device, notify::SYNC_DID_START)?;
    Ok(None)
}

fn end(device: &Device, afc: &mut Afc, lock: Option<u64>) -> bettertunes_device::Result<()> {
    if let Some(lock) = lock {
        afc.lock(lock, false)?;
        afc.close(lock)?;
    }
    notify::post(device, notify::SYNC_DID_FINISH)
}

/// On iPhone OS devices the FireWire GUID is the first 8 bytes of the UDID.
fn firewire_guid(udid: &str) -> Option<[u8; 8]> {
    let mut guid = [0u8; 8];
    for (i, b) in guid.iter_mut().enumerate() {
        *b = u8::from_str_radix(udid.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    (udid.len() == 40).then_some(guid)
}

fn db_track(t: &Track, id: u32, dest: &str, size: u64, kbps: Option<u32>, now: u64) -> DbTrack {
    let ext = dest.rsplit('.').next().unwrap_or_default();
    let kind = match (ext, kbps) {
        ("mp3", _) => "MPEG audio file",
        ("wav", _) => "WAV audio file",
        ("aif" | "aiff", _) => "AIFF audio file",
        (_, None) if t.bit_rate.is_some_and(|r| r > 500) => "Apple Lossless audio file",
        _ => "AAC audio file",
    };
    // Conversion resamples above 48 kHz to 44.1 or 48 kHz.
    let sample_rate = match (kbps, t.sample_rate) {
        (Some(_), Some(r)) if r > 48_000 => {
            if r % 44_100 == 0 {
                44_100
            } else {
                48_000
            }
        }
        (_, r) => r.unwrap_or(44_100),
    };
    DbTrack {
        id,
        dbid: t.persistent_id,
        title: t.title.clone(),
        artist: t.artist.clone(),
        album: t.album.clone(),
        album_artist: t.album_artist.clone(),
        genre: t.genre.clone(),
        composer: t.composer.clone(),
        kind: Some(kind.into()),
        location: dest.replace('/', ":"),
        size: size.min(u64::from(u32::MAX)) as u32,
        length_ms: t.duration_ms.unwrap_or(0),
        track_number: t.track_number.map_or(0, u32::from),
        track_count: t.track_count.map_or(0, u32::from),
        disc_number: t.disc_number.map_or(0, u32::from),
        disc_count: t.disc_count.map_or(0, u32::from),
        year: t.year.unwrap_or(0),
        bit_rate: kbps.or(t.bit_rate).unwrap_or(0),
        sample_rate,
        added: now,
        artwork: None,
    }
}

/// A stable 64-bit ID from a name (FNV-1a), for playlists without one.
fn name_id(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn name(t: &Track) -> String {
    crate::sync::track_name(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_firewire_guid_from_the_udid() {
        assert_eq!(
            firewire_guid("0123456789abcdef0123456789abcdef01234567"),
            Some([0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef])
        );
        assert_eq!(firewire_guid("00008000-0011223344556677"), None);
    }

    #[test]
    fn reads_the_devices_art_formats() {
        let xml = br#"<plist version="1.0"><array>
            <string>3001</string><dict><key>FormatId</key><integer>3001</integer>
              <key>PixelFormat</key><string>4C353535</string><key>PixelOrder</key><integer>1</integer>
              <key>RenderWidth</key><integer>256</integer><key>RenderHeight</key><integer>256</integer>
              <key>TopInset</key><integer>1</integer><key>BackColor</key><string>00000000</string>
              <key>RowBytesAlignment</key><integer>16</integer><key>OffsetAlignment</key><integer>4096</integer></dict>
            <string>3006</string><dict><key>FormatId</key><integer>3006</integer>
              <key>PixelFormat</key><string>4C353535</string><key>BackColor</key><string>FFFFFFFF</string>
              <key>RenderWidth</key><integer>55</integer><key>RenderHeight</key><integer>55</integer></dict>
            <string>3007</string><dict><key>FormatId</key><integer>3007</integer><key>AssociatedFormat</key><integer>2</integer>
              <key>PixelFormat</key><string>4C353535</string><key>RenderWidth</key><integer>88</integer><key>RenderHeight</key><integer>88</integer></dict>
            <string>1019</string><dict><key>FormatId</key><integer>1019</integer><key>PixelFormat</key><string>4C353635</string>
              <key>RenderWidth</key><integer>320</integer><key>RenderHeight</key><integer>320</integer></dict>
          </array></plist>"#;
        let formats = art_formats(&plist::from_bytes(xml).unwrap());
        assert_eq!(
            formats.iter().map(|f| f.id).collect::<Vec<_>>(),
            [3001, 3006]
        );
        assert!(formats[0].twiddled && formats[0].inset == 1);
        assert_eq!(formats[1].background, [255; 4]);
    }

    #[test]
    fn describes_tracks_for_the_database() {
        let t = Track {
            persistent_id: 0xAB,
            title: Some("Gila".into()),
            duration_ms: Some(300_000),
            bit_rate: Some(1411),
            sample_rate: Some(96_000),
            ..Track::default()
        };
        let d = db_track(
            &t,
            100,
            "/iTunes_Control/Music/F21/00000000000000AB.m4a",
            9_000_000,
            Some(256),
            0,
        );
        assert_eq!(d.location, ":iTunes_Control:Music:F21:00000000000000AB.m4a");
        assert_eq!((d.bit_rate, d.sample_rate), (256, 48_000));
        assert_eq!(d.kind.as_deref(), Some("AAC audio file"));
        let wav = db_track(&t, 101, "/x/a.wav", 1, None, 0);
        assert_eq!(
            (wav.kind.as_deref(), wav.sample_rate),
            (Some("WAV audio file"), 96_000)
        );
    }
}
