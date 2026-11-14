//! `bt sync`: choose what goes on each device, how it's converted, and see
//! whether it fits.

use bettertunes_core::profile::{self, Profile, Storage};
use bettertunes_core::sync::{
    self, Action, AlbumRef, Conversion, Plan, PlaylistRef, Remap, Selection, Skip, AAC_RATES,
};
use bettertunes_core::transcode::{self, Encoder};
use bettertunes_core::Library;
use bettertunes_device::{find_devices, ipod, model_name};
use clap::{Args, Subcommand};
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Mutex;

#[derive(Args)]
pub struct SyncArgs {
    /// Device to work on: UDID (or its start), FireWire GUID or name. Defaults
    /// to the connected device, or the only one set up.
    #[arg(short, long, global = true)]
    device: Option<String>,

    #[command(subcommand)]
    command: SyncCommand,
}

#[derive(Subcommand)]
enum SyncCommand {
    /// List the devices set up for syncing.
    List,
    /// Choose what goes on the device. With no options, asks interactively.
    Select(SelectArgs),
    /// Set conversion: `off`, or an AAC bit rate (128, 160, 192, 224, 256 or
    /// 320). Lossless songs and songs above the rate are converted.
    Convert {
        #[arg(value_name = "off|KBPS")]
        setting: String,
    },
    /// Show what would go on the device and whether it fits.
    Plan {
        /// List every song.
        #[arg(long)]
        tracks: bool,
        /// Don't check that song files exist (faster on network drives).
        #[arg(long)]
        no_check: bool,
    },
    /// Sync: put the chosen songs and playlists on the device, replacing
    /// what's there. Works for the first iPod touch (iPhone OS 1.x) so far.
    Run,
    /// Convert the songs that need it ahead of syncing, into the cache.
    Prepare {
        /// Conversions to run at once; defaults to the number of CPU cores.
        #[arg(short, long)]
        jobs: Option<usize>,
    },
}

#[derive(Args)]
struct SelectArgs {
    /// Sync the whole library.
    #[arg(long, conflicts_with = "none")]
    all: bool,
    /// Clear the selection first.
    #[arg(long)]
    none: bool,
    /// Remove the named items instead of adding them.
    #[arg(long)]
    remove: bool,
    #[arg(short, long, value_name = "NAME")]
    playlist: Vec<String>,
    #[arg(long, value_name = "NAME")]
    artist: Vec<String>,
    /// Album name, optionally as "ARTIST/ALBUM".
    #[arg(long, value_name = "NAME")]
    album: Vec<String>,
    #[arg(long, value_name = "NAME")]
    genre: Vec<String>,
}

impl SelectArgs {
    fn is_empty(&self) -> bool {
        !self.all
            && !self.none
            && self.playlist.is_empty()
            && self.artist.is_empty()
            && self.album.is_empty()
            && self.genre.is_empty()
    }
}

/// Runs a `bt sync` command. `library` opens the library when needed.
pub fn run(
    args: SyncArgs,
    remap: &Remap,
    library: impl FnOnce() -> Option<Library>,
    out: &mut impl Write,
) -> io::Result<ExitCode> {
    if let SyncCommand::List = args.command {
        return list(out);
    }
    let refresh = matches!(
        args.command,
        SyncCommand::Plan { .. } | SyncCommand::Select(_)
    );
    let Some(mut profile) = resolve(args.device.as_deref(), refresh) else {
        return Ok(ExitCode::FAILURE);
    };
    if let SyncCommand::Convert { setting } = &args.command {
        let Some(conversion) = parse_conversion(setting) else {
            eprintln!("bt: conversion is `off` or one of {AAC_RATES:?} kbps");
            return Ok(ExitCode::FAILURE);
        };
        profile.conversion = conversion;
        save(&profile)?;
        writeln!(out, "{}: {}", profile.name, describe_conversion(conversion))?;
        return Ok(ExitCode::SUCCESS);
    }
    let Some(lib) = library() else {
        return Ok(ExitCode::FAILURE);
    };
    match args.command {
        SyncCommand::Select(sel) if sel.is_empty() => {
            if !io::stdin().is_terminal() {
                eprintln!("bt: no terminal to ask in; choose with --playlist, --artist and so on");
                return Ok(ExitCode::FAILURE);
            }
            if !pick(&lib, &mut profile)? {
                return Ok(ExitCode::SUCCESS);
            }
            save(&profile)?;
            summary(&lib, &profile, remap, true, out)
        }
        SyncCommand::Select(sel) => {
            if !apply_selection(&lib, &mut profile.selection, &sel) {
                return Ok(ExitCode::FAILURE);
            }
            save(&profile)?;
            summary(&lib, &profile, remap, false, out)
        }
        SyncCommand::Plan { tracks, no_check } => {
            // Keep the fresh storage figures for planning while disconnected.
            save(&profile)?;
            let code = summary(&lib, &profile, remap, !no_check, out)?;
            if tracks {
                let plan = sync::plan(
                    &lib,
                    &profile.selection,
                    profile.conversion,
                    remap,
                    !no_check,
                );
                list_tracks(&plan, out)?;
            }
            Ok(code)
        }
        SyncCommand::Prepare { jobs } => prepare(&lib, &profile, remap, jobs, out),
        SyncCommand::Run => run_sync(&lib, &profile, remap, out),
        SyncCommand::List | SyncCommand::Convert { .. } => unreachable!(),
    }
}

fn save(profile: &Profile) -> io::Result<()> {
    profile.save().map_err(io::Error::other)
}

fn list(out: &mut impl Write) -> io::Result<ExitCode> {
    let profiles = Profile::all().map_err(io::Error::other)?;
    if profiles.is_empty() {
        writeln!(
            out,
            "No devices set up yet; connect one and run `bt sync select`."
        )?;
    }
    for p in profiles {
        writeln!(out, "{} ({}), {}", p.name, p.model, p.id)?;
        writeln!(out, "    {}", describe_selection(&p.selection))?;
        writeln!(out, "    {}", describe_conversion(p.conversion))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// A device seen on the bus or on disk.
pub(crate) struct Seen {
    pub id: String,
    pub name: String,
    pub model: String,
    /// Reads the storage figures, which can take a while.
    pub storage: Box<dyn FnOnce() -> Option<Storage> + Send>,
}

pub(crate) fn connected() -> Vec<Seen> {
    let mut seen = Vec::new();
    for d in find_devices(|_| {}) {
        if d.connection() == "Network" {
            continue;
        }
        let Ok(mut session) = d.session() else {
            continue;
        };
        let mut value = |key| {
            session
                .get_value(key)
                .ok()
                .flatten()
                .and_then(|v| v.as_string().map(str::to_owned))
        };
        let product = value("ProductType").unwrap_or_default();
        let name = value("DeviceName").unwrap_or_else(|| product.clone());
        let model = model_name(&product).map_or(product, str::to_owned);
        seen.push(Seen {
            id: d.udid.clone(),
            name,
            model,
            storage: Box::new(move || {
                let (total, free, music) = d.storage().ok()?;
                Some(Storage { total, free, music })
            }),
        });
    }
    for pod in ipod::find() {
        seen.push(Seen {
            id: pod.id(),
            name: pod.name(),
            model: format!(
                "iPod ({})",
                pod.model_number.as_deref().unwrap_or("unknown model")
            ),
            storage: Box::new(move || {
                let (total, free, music) = pod.storage().ok()?;
                Some(Storage { total, free, music })
            }),
        });
    }
    seen
}

/// Finds the profile to work on, creating it for a newly seen device. With
/// `refresh`, re-reads the storage figures of a connected device.
pub(crate) fn resolve(wanted: Option<&str>, refresh: bool) -> Option<Profile> {
    let matches = |id: &str, name: &str| {
        wanted.is_none_or(|w| id.starts_with(w) || name.eq_ignore_ascii_case(w))
    };
    let mut seen: Vec<Seen> = connected()
        .into_iter()
        .filter(|s| matches(&s.id, &s.name))
        .collect();
    let profiles = Profile::all().unwrap_or_default();
    match seen.len() {
        1 => {
            let s = seen.pop().unwrap();
            let mut p = profiles
                .into_iter()
                .find(|p| p.id == s.id)
                .unwrap_or_default();
            p.id.clone_from(&s.id);
            p.name.clone_from(&s.name);
            p.model.clone_from(&s.model);
            if refresh || p.storage.is_none() {
                p.storage = (s.storage)().or(p.storage);
            }
            Some(p)
        }
        0 => {
            let mut known: Vec<_> = profiles
                .into_iter()
                .filter(|p| matches(&p.id, &p.name))
                .collect();
            if known.len() == 1 {
                return known.pop();
            }
            if known.is_empty() {
                eprintln!("bt: no matching device connected or set up (see `bt devices`)");
            } else {
                eprintln!("bt: several devices set up; pick one with --device:");
                known
                    .iter()
                    .for_each(|p| eprintln!("    {} ({})", p.name, p.id));
            }
            None
        }
        _ => {
            eprintln!("bt: several devices connected; pick one with --device:");
            seen.iter()
                .for_each(|s| eprintln!("    {} ({})", s.name, s.id));
            None
        }
    }
}

fn parse_conversion(s: &str) -> Option<Conversion> {
    if s.eq_ignore_ascii_case("off") {
        return Some(Conversion::Off);
    }
    let kbps = s
        .trim_end_matches("kbps")
        .trim_end_matches('k')
        .parse()
        .ok()?;
    AAC_RATES
        .contains(&kbps)
        .then_some(Conversion::Aac { kbps })
}

pub(crate) fn describe_conversion(c: Conversion) -> String {
    match c {
        Conversion::Off => {
            "No conversion (only formats the device can't play are converted)".into()
        }
        Conversion::Aac { kbps } => {
            format!("Convert lossless and higher bit rate songs to {kbps} kbps AAC")
        }
    }
}

pub(crate) fn describe_selection(s: &Selection) -> String {
    if s.everything {
        return "Entire library".into();
    }
    if s.is_empty() {
        return "Nothing selected".into();
    }
    let quoted = |v: Vec<&str>| {
        v.iter()
            .map(|n| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut parts = Vec::new();
    let mut part = |label: &str, names: Vec<&str>| {
        if !names.is_empty() {
            parts.push(format!("{label}: {}", quoted(names)));
        }
    };
    part(
        "playlists",
        s.playlists.iter().map(|p| p.name.as_str()).collect(),
    );
    part("artists", s.artists.iter().map(String::as_str).collect());
    part(
        "albums",
        s.albums.iter().map(|a| a.album.as_str()).collect(),
    );
    part("genres", s.genres.iter().map(String::as_str).collect());
    parts.join("; ")
}

/// Applies `select` options. Returns false (after saying why) if a name
/// isn't in the library.
fn apply_selection(lib: &Library, sel: &mut Selection, args: &SelectArgs) -> bool {
    if args.none {
        *sel = Selection::default();
    }
    if args.all {
        sel.everything = true;
    }
    let mut ok = true;
    let unknown = |kind: &str, name: &str| {
        eprintln!("bt: no {kind} named {name:?} in the library");
        false
    };
    for name in &args.playlist {
        let found = lib.find_playlists(name);
        let same_name = found.iter().all(|p| p.name == found[0].name);
        match found[..] {
            [] => ok = unknown("playlist", name),
            [p] => toggle(&mut sel.playlists, PlaylistRef::new(p), args.remove),
            _ if !same_name => {
                eprintln!("bt: {name:?} matches several playlists; use more of the name or the ID from `bt playlists`:");
                for p in &found {
                    eprintln!(
                        "    {}  {}",
                        p.persistent_id
                            .map_or_else(String::new, |id| format!("{id:016X}")),
                        p.name
                    );
                }
                ok = false;
            }
            _ => {
                eprintln!("bt: several playlists named {name:?}; adding all of them");
                found
                    .into_iter()
                    .for_each(|p| toggle(&mut sel.playlists, PlaylistRef::new(p), args.remove));
            }
        }
    }
    for name in &args.artist {
        let known = lib
            .tracks
            .iter()
            .any(|t| t.artist.as_ref() == Some(name) || t.album_artist.as_ref() == Some(name));
        if known || args.remove {
            toggle(&mut sel.artists, name.clone(), args.remove);
        } else {
            ok = unknown("artist", name);
        }
    }
    for name in &args.album {
        let album = match name.split_once('/') {
            Some((artist, album)) => AlbumRef {
                album: album.into(),
                artist: Some(artist.into()),
            },
            None => AlbumRef {
                album: name.clone(),
                artist: None,
            },
        };
        if args.remove || lib.tracks.iter().any(|t| album.matches(t)) {
            toggle(&mut sel.albums, album, args.remove);
        } else {
            ok = unknown("album", name);
        }
    }
    for name in &args.genre {
        if args.remove || lib.tracks.iter().any(|t| t.genre.as_ref() == Some(name)) {
            toggle(&mut sel.genres, name.clone(), args.remove);
        } else {
            ok = unknown("genre", name);
        }
    }
    if !args.playlist.is_empty()
        || !args.artist.is_empty()
        || !args.album.is_empty()
        || !args.genre.is_empty()
    {
        // Naming items means syncing just those.
        sel.everything = sel.everything && args.all;
    }
    ok
}

fn toggle<T: PartialEq>(list: &mut Vec<T>, item: T, remove: bool) {
    if remove {
        list.retain(|x| *x != item);
    } else if !list.contains(&item) {
        list.push(item);
    }
}

/// Asks for the conversion and the playlists. Returns false if cancelled.
fn pick(lib: &Library, profile: &mut Profile) -> io::Result<bool> {
    use dialoguer::{theme::ColorfulTheme, MultiSelect, Select};
    let theme = ColorfulTheme::default();
    let ask = |e: dialoguer::Error| match e {
        dialoguer::Error::IO(e) => e,
    };

    let mut options = vec!["Keep songs as they are".to_owned()];
    options.extend(AAC_RATES.iter().map(|r| format!("Convert to {r} kbps AAC")));
    let current = match profile.conversion {
        Conversion::Off => 0,
        Conversion::Aac { kbps } => AAC_RATES
            .iter()
            .position(|&r| r == kbps)
            .map_or(0, |i| i + 1),
    };
    let Some(choice) = Select::with_theme(&theme)
        .with_prompt("Conversion (applies to lossless and higher bit rate songs)")
        .items(&options)
        .default(current)
        .interact_opt()
        .map_err(ask)?
    else {
        return Ok(false);
    };
    profile.conversion = match choice {
        0 => Conversion::Off,
        i => Conversion::Aac {
            kbps: AAC_RATES[i - 1],
        },
    };

    let playlists: Vec<_> = lib.playlists.iter().filter(|p| p.is_user()).collect();
    let size_of = |sel: &Selection| {
        sync::plan(lib, sel, profile.conversion, &Remap::default(), false).total_size()
    };
    let everything = Selection {
        everything: true,
        ..Selection::default()
    };
    let mut items = vec![format!(
        "Entire library ({} songs, {})",
        lib.tracks.len(),
        size(size_of(&everything))
    )];
    let mut checked = vec![profile.selection.everything];
    for p in &playlists {
        let one = Selection {
            playlists: vec![PlaylistRef::new(p)],
            ..Selection::default()
        };
        let smart = if p.is_smart { ", smart" } else { "" };
        items.push(format!(
            "{} ({} songs, {}{smart})",
            p.name,
            p.track_ids.len(),
            size(size_of(&one))
        ));
        checked.push(profile.selection.playlists.iter().any(|r| r.matches(p)));
    }
    if let Some(room) = profile.storage.map(|s| s.room_for_music()) {
        eprintln!("Room for music on {}: {}", profile.name, size(room));
    }
    let Some(chosen) = MultiSelect::with_theme(&theme)
        .with_prompt("Playlists to sync (space to toggle, enter to confirm)")
        .items(&items)
        .defaults(&checked)
        .max_length(20)
        .interact_opt()
        .map_err(ask)?
    else {
        return Ok(false);
    };
    profile.selection.everything = chosen.contains(&0);
    profile.selection.playlists = chosen
        .iter()
        .filter(|&&i| i > 0)
        .map(|&i| PlaylistRef::new(playlists[i - 1]))
        .collect();
    Ok(true)
}

fn summary(
    lib: &Library,
    profile: &Profile,
    remap: &Remap,
    check_files: bool,
    out: &mut impl Write,
) -> io::Result<ExitCode> {
    let plan = sync::plan(
        lib,
        &profile.selection,
        profile.conversion,
        remap,
        check_files,
    );
    writeln!(out, "{} ({}), {}", profile.name, profile.model, profile.id)?;
    writeln!(
        out,
        "Selection:  {}",
        describe_selection(&profile.selection)
    )?;
    writeln!(
        out,
        "Conversion: {}",
        describe_conversion(profile.conversion)
    )?;
    for r in &plan.missing_playlists {
        writeln!(
            out,
            "Warning:    playlist \"{}\" is no longer in the library",
            r.name
        )?;
    }
    let converted: Vec<_> = plan.converted().collect();
    let total = plan.total_size();
    writeln!(
        out,
        "Plan:       {} songs, {}{}",
        plan.items.len(),
        size(total),
        if converted.is_empty() {
            String::new()
        } else {
            format!(
                " ({} converted, {})",
                converted.len(),
                size(converted.iter().map(|i| i.size).sum())
            )
        }
    )?;
    if !plan.playlists.is_empty() {
        writeln!(out, "            {} playlists", plan.playlists.len())?;
    }
    if !plan.skipped.is_empty() {
        let mut reasons: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, skip) in &plan.skipped {
            *reasons.entry(skip.describe()).or_default() += 1;
        }
        let reasons: Vec<_> = reasons.iter().map(|(r, n)| format!("{n} {r}")).collect();
        writeln!(out, "Skipped:    {}", reasons.join(", "))?;
    }
    let Some(storage) = profile.storage else {
        writeln!(
            out,
            "Storage:    unknown (connect the device to check it fits)"
        )?;
        return Ok(ExitCode::SUCCESS);
    };
    let room = storage.room_for_music();
    writeln!(
        out,
        "Device:     {} total, {} room for music",
        size(storage.total),
        size(room)
    )?;
    if total <= room {
        writeln!(out, "Fits, with {} to spare.", size(room - total))?;
        return Ok(ExitCode::SUCCESS);
    }
    writeln!(out, "Does NOT fit: {} too much.", size(total - room))?;
    // Show what lower bit rates would do.
    for &kbps in AAC_RATES.iter().rev() {
        let current = match profile.conversion {
            Conversion::Aac { kbps: k } => k,
            Conversion::Off => u32::MAX,
        };
        if kbps >= current {
            continue;
        }
        let at = sync::plan(
            lib,
            &profile.selection,
            Conversion::Aac { kbps },
            remap,
            false,
        )
        .total_size();
        let verdict = if at <= room { "fits" } else { "too big" };
        writeln!(out, "    at {kbps} kbps AAC: {} ({verdict})", size(at))?;
    }
    Ok(ExitCode::FAILURE)
}

fn list_tracks(plan: &Plan, out: &mut impl Write) -> io::Result<()> {
    writeln!(out)?;
    for item in &plan.items {
        let action = match item.action {
            Action::Copy => "copy       ".to_owned(),
            Action::Convert { kbps } => format!("aac {kbps:<7}"),
        };
        writeln!(
            out,
            "{action} {}  ({})",
            track_name(item.track),
            size(item.size)
        )?;
    }
    for (t, skip) in &plan.skipped {
        writeln!(out, "skip        {}  ({})", track_name(t), skip.describe())?;
    }
    Ok(())
}

fn prepare(
    lib: &Library,
    profile: &Profile,
    remap: &Remap,
    jobs: Option<usize>,
    out: &mut impl Write,
) -> io::Result<ExitCode> {
    let plan = sync::plan(lib, &profile.selection, profile.conversion, remap, true);
    let missing = plan
        .skipped
        .iter()
        .filter(|(_, s)| *s == Skip::Missing)
        .count();
    if missing > 0 {
        eprintln!(
            "bt: {missing} song files not found (see `bt sync plan --tracks`; --remap may help)"
        );
    }
    let cache = profile::cache_dir().map_err(io::Error::other)?;
    let encoders = Encoder::available();
    if encoders.is_empty() && plan.converted().next().is_some() {
        eprintln!("bt: no AAC encoder found; install ffmpeg");
        return Ok(ExitCode::FAILURE);
    }
    let (todo, cached) = transcode::jobs(&plan, &cache, remap);
    if todo.is_empty() {
        writeln!(out, "Nothing to convert ({cached} already in the cache).")?;
        return Ok(ExitCode::SUCCESS);
    }
    let threads = jobs
        .unwrap_or_else(transcode::default_threads)
        .clamp(1, todo.len());
    writeln!(
        out,
        "Converting {} songs with {}, {threads} at a time ({cached} already in the cache)",
        todo.len(),
        encoders.first().map_or_else(String::new, Encoder::describe)
    )?;
    let failures = Mutex::new(Vec::new());
    transcode::run(&todo, &encoders, threads, |n, job, result| {
        eprintln!("[{n}/{}] {}", todo.len(), track_name(job.track));
        if let Err(e) = result {
            failures.lock().unwrap().push(e);
        }
        true
    });
    let failures = failures.into_inner().unwrap();
    writeln!(
        out,
        "Converted {} songs into {}",
        todo.len() - failures.len(),
        cache.display()
    )?;
    if failures.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    writeln!(out, "{} failed:", failures.len())?;
    for f in &failures {
        writeln!(out, "    {f}")?;
    }
    Ok(ExitCode::FAILURE)
}

fn run_sync(
    lib: &Library,
    profile: &Profile,
    remap: &Remap,
    out: &mut impl Write,
) -> io::Result<ExitCode> {
    let Some(device) = crate::push::find(&profile.id) else {
        eprintln!("bt: {} isn't connected over USB", profile.name);
        return Ok(ExitCode::FAILURE);
    };
    writeln!(
        out,
        "Syncing {} ({})",
        profile.name,
        describe_selection(&profile.selection)
    )?;
    let progress = |e: crate::push::Event| match e {
        crate::push::Event::Step(s) => eprintln!("{s}..."),
        crate::push::Event::Converting { done, total } => {
            eprintln!("  converted {done} of {total}")
        }
        crate::push::Event::Copying { done, total, track } => {
            eprintln!("  [{}/{total}] {}", done + 1, track_name(track))
        }
    };
    match crate::push::run(&device, lib, profile, remap, &progress) {
        Ok(r) => {
            writeln!(
                out,
                "Done: {} songs and {} playlists on {}. Copied {} ({}), kept {}, removed {}.",
                r.songs,
                r.playlists,
                profile.name,
                r.copied,
                size(r.bytes),
                r.kept,
                r.removed
            )?;
            for f in &r.failed {
                writeln!(out, "    not synced: {f}")?;
            }
            for w in &r.warnings {
                writeln!(out, "Warning: {w}")?;
            }
            Ok(if r.failed.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Err(e) => {
            eprintln!("bt: sync failed: {e}");
            Ok(ExitCode::FAILURE)
        }
    }
}

pub(crate) fn track_name(t: &bettertunes_core::Track) -> String {
    format!(
        "{} - {}",
        t.artist.as_deref().unwrap_or("Unknown Artist"),
        t.title.as_deref().unwrap_or("(untitled)")
    )
}

/// Decimal units, as Apple uses for storage.
pub(crate) fn size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else {
        format!("{:.0} MB", b / 1e6)
    }
}
