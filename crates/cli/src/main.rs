use bettertunes_core::profile::{self, PathMap, Settings};
use bettertunes_core::sync::Remap;
use bettertunes_core::{clones, Library, Track};
use bettertunes_device::{find_devices, ipod, lockdown::Lockdown, model_name, Device};
use clap::{Parser, Subcommand};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod device;
mod pull;
mod push;
mod sync;
mod tui;

/// betterTunes: manage your music library and sync it to iPods and iPhones.
#[derive(Parser)]
#[command(name = "bt", version)]
struct Cli {
    /// Library to open: iTunes Library.itl, a .musiclibrary bundle,
    /// Library.musicdb, or an iTunes XML export. Defaults to the usual
    /// Music/iTunes locations.
    #[arg(short, long, global = true)]
    library: Option<PathBuf>,

    /// Rewrite song paths from FROM to TO, for a library made on another Mac
    /// (e.g. `/Users/me=/Volumes/Mac/Users/me`).
    #[arg(long, value_name = "FROM=TO", global = true, value_parser = parse_remap)]
    remap: Vec<(PathBuf, PathBuf)>,

    /// Without a command, opens the full-screen view for choosing what to
    /// sync.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List connected iPhones, iPod touches and mounted iPods.
    Devices,
    /// Pair an iPod touch or iPhone that only betterTunes can reach (such as
    /// a first-generation iPod touch on current macOS). This stores
    /// betterTunes' certificates on the device, as iTunes does.
    Pair {
        /// UDID of the device; needed only when several are connected.
        udid: Option<String>,
    },
    /// Choose what to sync to a device (playlists, artists, albums, genres
    /// or everything) and whether to convert songs to AAC, and check it fits.
    Sync(sync::SyncArgs),
    /// Bring changes made on a device back into iTunes: songs added to
    /// playlists on an iPhone (merging the "Name 1" copies iTunes makes),
    /// and play counts and ratings from an iPod touch.
    Pull {
        /// Device: UDID (or its start) or name.
        #[arg(short, long)]
        device: Option<String>,
        /// Save the AppleScript here instead of running it.
        #[arg(long, value_name = "FILE")]
        script: Option<PathBuf>,
        /// Apply without asking.
        #[arg(long)]
        yes: bool,
    },
    /// Back up or clear the music on an iPhone or iPod touch.
    Device(device::DeviceArgs),
    /// Show or choose the library to use, e.g. another Mac's mounted over
    /// the network: `bt library /Volumes/macOS/Users/me/Music/iTunes`.
    Library {
        /// iTunes folder or library file. Without it, shows the current one.
        path: Option<PathBuf>,
        /// Go back to this computer's own library.
        #[arg(long, conflicts_with = "path")]
        reset: bool,
    },
    /// Show a summary of the library.
    Info,
    /// List tracks.
    Tracks {
        /// Only tracks backed by a local file (the ones that can be synced).
        #[arg(long)]
        local: bool,
    },
    /// List your playlists (with IDs and lengths), or the tracks of one.
    Playlists {
        /// Playlist to show the tracks of: name, part of the name, or ID.
        name: Option<String>,
        /// Order of the list.
        #[arg(long, value_enum, default_value_t = SortBy::Library)]
        sort: SortBy,
        /// Include iTunes' built-in playlists (Music, Films, Podcasts...).
        #[arg(long)]
        all: bool,
    },
    /// Find "Name 1" copies that iTunes made of playlists edited on an
    /// iPhone/iPod, and optionally write an AppleScript that merges them.
    Clones {
        /// Leave out the pair whose original (or copy) has this name.
        #[arg(long, value_name = "NAME")]
        exclude: Vec<String>,
        /// Write the merge AppleScript to this file (run it on the Mac with
        /// iTunes, e.g. `osascript FILE`).
        #[arg(long, value_name = "FILE")]
        script: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum SortBy {
    /// As in iTunes.
    Library,
    Name,
    Songs,
    Length,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        if !io::stdout().is_terminal() {
            eprintln!("bt: no terminal for the full-screen view; see `bt --help`");
            return ExitCode::FAILURE;
        }
        let Some((lib, path)) = open_library(cli.library) else {
            return ExitCode::FAILURE;
        };
        return match tui::run(&lib, &path, &remap(cli.remap)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("bt: {e}");
                ExitCode::FAILURE
            }
        };
    };
    let out = &mut io::stdout().lock();
    let result = match command {
        Command::Devices => list_devices(out),
        Command::Pair { udid } => pair(udid.as_deref(), out),
        Command::Device(args) => device::run(args, out),
        Command::Pull {
            device,
            script,
            yes,
        } => (|| {
            let Some(profile) = sync::resolve(device.as_deref(), false) else {
                return Ok(ExitCode::FAILURE);
            };
            let Some(dev) = push::find(&profile.id) else {
                eprintln!("bt: {} isn't connected over USB", profile.name);
                return Ok(ExitCode::FAILURE);
            };
            let Some((lib, _)) = open_library(cli.library) else {
                return Ok(ExitCode::FAILURE);
            };
            pull::run(&dev, &profile, &lib, script, yes, out)
        })(),
        Command::Library { path, reset } => set_library(path, reset, cli.remap, out),
        Command::Sync(args) => sync::run(
            args,
            &remap(cli.remap),
            || open_library(cli.library).map(|(l, _)| l),
            out,
        ),
        command => {
            let Some((lib, path)) = open_library(cli.library) else {
                return ExitCode::FAILURE;
            };
            report(&lib, &path, command, out)
        }
    };
    match result {
        Ok(code) => code,
        // Output piped into e.g. `head`, which exited early.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bt: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_remap(s: &str) -> Result<(PathBuf, PathBuf), String> {
    let (from, to) = s.split_once('=').ok_or("expected FROM=TO")?;
    Ok((from.into(), to.into()))
}

/// The library to use: `--library`, else the one set with `bt library`,
/// else this computer's own. A set library that isn't there (another Mac's
/// disk not mounted) is an error rather than a reason to use this one's.
fn library_path(arg: Option<PathBuf>) -> Option<PathBuf> {
    if arg.is_some() {
        return arg;
    }
    if let Some(path) = Settings::load().unwrap_or_default().library {
        if path.exists() {
            return Some(path);
        }
        eprintln!(
            "bt: the library set with `bt library` isn't there: {}\n    \
             Is the other Mac's disk mounted? (`bt library --reset` uses this computer's)",
            path.display()
        );
        return None;
    }
    let found = Library::default_paths().into_iter().find(|p| p.exists());
    if found.is_none() {
        eprintln!("bt: no library found; choose one with `bt library PATH`");
    }
    found
}

/// `--remap` options, else the mapping saved with `bt library`.
fn remap(args: Vec<(PathBuf, PathBuf)>) -> Remap {
    if !args.is_empty() {
        return Remap(args);
    }
    let saved = Settings::load().unwrap_or_default().remap;
    Remap(saved.into_iter().map(|m| (m.from, m.to)).collect())
}

/// `bt library`: shows, sets or resets the library to use.
fn set_library(
    path: Option<PathBuf>,
    reset: bool,
    remap_args: Vec<(PathBuf, PathBuf)>,
    out: &mut impl Write,
) -> io::Result<ExitCode> {
    let mut settings = Settings::load().map_err(io::Error::other)?;
    if reset {
        settings = Settings::default();
        settings.save().map_err(io::Error::other)?;
        writeln!(out, "Using this computer's own library again.")?;
        return Ok(ExitCode::SUCCESS);
    }
    let Some(path) = path else {
        let (source, path) = match &settings.library {
            Some(p) => ("set with `bt library`", Some(p.clone())),
            None => (
                "this computer's",
                Library::default_paths().into_iter().find(|p| p.exists()),
            ),
        };
        let Some(path) = path else {
            writeln!(out, "No library found; choose one with `bt library PATH`.")?;
            return Ok(ExitCode::SUCCESS);
        };
        writeln!(out, "Library ({source}): {}", path.display())?;
        for m in &settings.remap {
            writeln!(
                out,
                "Song files: {} is read from {}",
                m.from.display(),
                m.to.display()
            )?;
        }
        if !path.exists() {
            writeln!(
                out,
                "Not available right now: is the other Mac's disk mounted?"
            )?;
        }
        return Ok(ExitCode::SUCCESS);
    };
    let path = std::path::absolute(&path)?;
    let lib = match Library::open(&path) {
        Ok(lib) => lib,
        Err(e) => {
            eprintln!("bt: {}: {e}", path.display());
            return Ok(ExitCode::FAILURE);
        }
    };
    // Given mappings win over the guess from where the library is mounted.
    let maps: Vec<PathMap> = if remap_args.is_empty() {
        profile::guess_remap(&path).into_iter().collect()
    } else {
        remap_args
            .into_iter()
            .map(|(from, to)| PathMap { from, to })
            .collect()
    };
    let remap = Remap(
        maps.iter()
            .map(|m| (m.from.clone(), m.to.clone()))
            .collect(),
    );
    // Check a sample of songs can be found here.
    let sample: Vec<_> = lib
        .tracks
        .iter()
        .filter_map(|t| t.location.as_ref())
        .take(50)
        .collect();
    let found = sample.iter().filter(|p| remap.apply(p).is_file()).count();
    settings.library = Some(path.clone());
    settings.remap = maps;
    settings.save().map_err(io::Error::other)?;
    let playlists = lib.playlists.iter().filter(|p| p.is_user()).count();
    writeln!(
        out,
        "Using {} ({} songs, {playlists} playlists).",
        path.display(),
        lib.tracks.len()
    )?;
    for m in &settings.remap {
        writeln!(
            out,
            "Song files under {} on that Mac are read from {}.",
            m.from.display(),
            m.to.display()
        )?;
    }
    if found < sample.len() {
        writeln!(
            out,
            "Warning: only {found} of {} sampled song files were found here; \
             use --remap FROM=TO if they live elsewhere.",
            sample.len()
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

fn open_library(path: Option<PathBuf>) -> Option<(Library, PathBuf)> {
    let path = library_path(path)?;
    match Library::open(&path) {
        Ok(lib) => Some((lib, path)),
        Err(e) => {
            eprintln!("bt: {}: {e}", path.display());
            None
        }
    }
}

/// "Model, iOS version, "name"", read inside a session when the device is
/// paired. Unpaired old firmware reveals little more than its name.
fn device_summary(d: &Device) -> bettertunes_device::Result<String> {
    let (mut l, paired) = match d.session() {
        Ok(l) => (l, true),
        Err(_) => (Lockdown::connect(d)?, false),
    };
    let mut value = |key| -> bettertunes_device::Result<Option<String>> {
        match l.get_value(key) {
            Ok(v) => Ok(v.and_then(|v| v.as_string().map(str::to_owned))),
            Err(bettertunes_device::Error::Refused(_)) => Ok(None),
            Err(e) => Err(e),
        }
    };
    let product = value("ProductType")?;
    let version = value("ProductVersion")?;
    let name = value("DeviceName")?;
    let mut s = match product {
        Some(p) => model_name(&p).map_or_else(|| p.clone(), |m| format!("{m} ({p})")),
        None => value("DeviceClass")?.unwrap_or_else(|| "iOS device".into()),
    };
    if let Some(v) = version {
        s += &format!(", iOS {v}");
    }
    if let Some(name) = name {
        s += &format!(", \"{name}\"");
    }
    if !paired && d.connection().contains("direct") {
        s += " (not paired: see `bt pair`)";
    }
    Ok(s)
}

fn pair(udid: Option<&str>, out: &mut impl Write) -> io::Result<ExitCode> {
    let devices: Vec<_> = find_devices(|e| eprintln!("bt: {e}"))
        .into_iter()
        .filter(|d| udid.is_none_or(|u| d.udid == u))
        .collect();
    let [d] = &devices[..] else {
        eprintln!(
            "bt: {} matching devices; pass the UDID shown by `bt devices`",
            devices.len()
        );
        return Ok(ExitCode::FAILURE);
    };
    if d.pair_record().ok().flatten().is_some() {
        writeln!(out, "{} is already paired.", d.udid)?;
        return Ok(ExitCode::SUCCESS);
    }
    match d.pair() {
        Ok(()) => {
            writeln!(out, "Paired {}.", d.udid)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("bt: pairing {} failed: {e}", d.udid);
            Ok(ExitCode::FAILURE)
        }
    }
}

fn list_devices(out: &mut impl Write) -> io::Result<ExitCode> {
    let mut found = false;
    // Without usbmuxd there can still be iPods mounted as disks.
    for d in find_devices(|e| eprintln!("bt: {e}")) {
        found = true;
        match device_summary(&d) {
            Ok(summary) => writeln!(out, "{summary}")?,
            // Over Wi-Fi, lockdownd only answers inside a TLS session.
            Err(_) if d.connection() == "Network" => writeln!(
                out,
                "iOS device on Wi-Fi (connect it with USB to see details)"
            )?,
            Err(e) => writeln!(out, "iOS device (can't read details: {e})")?,
        }
        writeln!(out, "    UDID {}, {}", d.udid, d.connection())?;
    }
    for ipod in ipod::find() {
        found = true;
        let model = ipod.model_number.as_deref().unwrap_or("unknown model");
        writeln!(
            out,
            "iPod ({model})\n    mounted at {}",
            ipod.mount_point.display()
        )?;
    }
    if !found {
        writeln!(out, "No devices found.")?;
    }
    Ok(ExitCode::SUCCESS)
}

fn report(
    lib: &Library,
    path: &Path,
    command: Command,
    out: &mut impl Write,
) -> io::Result<ExitCode> {
    match command {
        Command::Info => {
            let local = lib.tracks.iter().filter(|t| t.location.is_some()).count();
            let playlists = lib.playlists.iter().filter(|p| !p.is_master).count();
            writeln!(out, "Library:   {}", path.display())?;
            writeln!(
                out,
                "Tracks:    {} ({local} local files, {} streaming/cloud only)",
                lib.tracks.len(),
                lib.tracks.len() - local
            )?;
            writeln!(out, "Playlists: {playlists}")?;
        }
        Command::Tracks { local } => {
            for t in lib.tracks.iter().filter(|t| !local || t.location.is_some()) {
                print_track(out, t, local)?;
            }
        }
        Command::Playlists {
            name: None,
            sort,
            all,
        } => {
            let mut list: Vec<_> = lib
                .playlists
                .iter()
                .filter(|p| if all { !p.is_master } else { p.is_user() })
                .map(|p| (p, lib.duration_ms(p)))
                .collect();
            match sort {
                SortBy::Library => {}
                SortBy::Name => list.sort_by_key(|(p, _)| p.name.to_lowercase()),
                SortBy::Songs => list.sort_by_key(|(p, _)| p.track_ids.len()),
                SortBy::Length => list.sort_by_key(|&(_, ms)| ms),
            }
            for (p, ms) in list {
                let id = p
                    .persistent_id
                    .map_or_else(|| " ".repeat(16), |id| format!("{id:016X}"));
                let smart = if p.is_smart { "  [smart]" } else { "" };
                writeln!(
                    out,
                    "{id}  {:>5} songs  {:>9}  {}{smart}",
                    p.track_ids.len(),
                    length(ms),
                    p.name
                )?;
            }
        }
        Command::Playlists {
            name: Some(name), ..
        } => {
            let found = lib.find_playlists(&name);
            let [p] = found[..] else {
                if found.is_empty() {
                    eprintln!("bt: no playlist matches {name:?}");
                } else {
                    eprintln!(
                        "bt: {name:?} matches several playlists; use more of the name or the ID:"
                    );
                    for p in found {
                        let id = p
                            .persistent_id
                            .map_or_else(String::new, |id| format!("{id:016X}"));
                        eprintln!("    {id}  {}", p.name);
                    }
                }
                return Ok(ExitCode::FAILURE);
            };
            for t in p.track_ids.iter().filter_map(|&id| lib.track(id)) {
                print_track(out, t, false)?;
            }
        }
        Command::Devices
        | Command::Pair { .. }
        | Command::Sync(_)
        | Command::Device(_)
        | Command::Library { .. }
        | Command::Pull { .. } => {
            unreachable!("handled separately")
        }
        Command::Clones { exclude, script } => {
            return report_clones(lib, &exclude, script.as_deref(), out)
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn report_clones(
    lib: &Library,
    exclude: &[String],
    script: Option<&Path>,
    out: &mut impl Write,
) -> io::Result<ExitCode> {
    let pairs: Vec<_> = clones::find(lib)
        .into_iter()
        .filter(|p| !exclude.contains(&p.original.name) && !exclude.contains(&p.copy.name))
        .collect();
    if pairs.is_empty() {
        writeln!(out, "No playlist copies found.")?;
        return Ok(ExitCode::SUCCESS);
    }
    let name = |id: &u64| {
        lib.track(*id).map_or_else(
            || format!("{id:016X}"),
            |t| {
                let artist = t.artist.as_deref().unwrap_or("Unknown Artist");
                format!("{artist} - {}", t.title.as_deref().unwrap_or("(untitled)"))
            },
        )
    };
    for p in &pairs {
        writeln!(out, "{}  <-  {}", p.original.name, p.copy.name)?;
        writeln!(out, "    adds {} from the copy", p.added.len())?;
        for id in &p.added {
            writeln!(out, "      + {}", name(id))?;
        }
        if !p.missing.is_empty() {
            writeln!(out, "    keeps {} not in the copy", p.missing.len())?;
            for id in &p.missing {
                writeln!(out, "      = {}", name(id))?;
            }
        }
    }
    if let Some(path) = script {
        std::fs::write(path, clones::merge_script(&pairs))?;
        writeln!(
            out,
            "\nWrote merge script for {} pairs to {}",
            pairs.len(),
            path.display()
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

/// "h:mm:ss", or "m:ss" under an hour.
fn length(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn print_track(out: &mut impl Write, t: &Track, with_path: bool) -> io::Result<()> {
    let title = t.title.as_deref().unwrap_or("(untitled)");
    let artist = t.artist.as_deref().unwrap_or("Unknown Artist");
    let mut line = format!("{artist} - {title}");
    if let Some(album) = &t.album {
        line += &format!("  [{album}]");
    }
    if let Some(ms) = t.duration_ms {
        line += &format!("  {}:{:02}", ms / 60_000, ms / 1000 % 60);
    }
    if with_path {
        if let Some(path) = &t.location {
            line += &format!("\n    {}", path.display());
        }
    }
    writeln!(out, "{line}")
}
