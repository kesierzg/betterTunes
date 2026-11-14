//! `bt device`: back up and clear the music on an iPhone or iPod touch.

use bettertunes_core::profile;
use bettertunes_device::afc::{self, Afc};
use bettertunes_device::{find_devices, Device};
use clap::{Args, Subcommand};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Args)]
pub struct DeviceArgs {
    /// UDID (or its start) of the device; needed only when several are
    /// connected.
    #[arg(short, long, global = true)]
    device: Option<String>,

    #[command(subcommand)]
    command: DeviceCommand,
}

#[derive(Subcommand)]
enum DeviceCommand {
    /// Copy the device's music, database and artwork (`iTunes_Control`) to
    /// this computer.
    Backup,
    /// Remove all synced music, its database and artwork. Asks whether to
    /// back it up first. Settings, photos and the iTunes preferences stay.
    EraseMusic {
        /// Don't ask for confirmation.
        #[arg(long)]
        yes: bool,
        /// Back up first without asking.
        #[arg(long, conflicts_with = "no_backup")]
        backup: bool,
        /// Don't back up, without asking.
        #[arg(long)]
        no_backup: bool,
    },
}

/// Removed by `erase-music`, besides the song files themselves.
const MUSIC_DATA: [&str; 2] = [
    "/iTunes_Control/iTunes/iTunesDB",
    "/com.apple.itunes.syncing",
];

pub fn run(args: DeviceArgs, out: &mut impl Write) -> io::Result<ExitCode> {
    let Some(device) = pick(args.device.as_deref()) else {
        return Ok(ExitCode::FAILURE);
    };
    let mut afc = Afc::new(
        device
            .start_service(afc::SERVICE)
            .map_err(io::Error::other)?,
    );
    match args.command {
        DeviceCommand::Backup => {
            let dir = backup(&device, &mut afc, out)?;
            writeln!(out, "Backed up to {}", dir.display())?;
        }
        DeviceCommand::EraseMusic {
            yes,
            backup: always,
            no_backup,
        } => {
            let entries = afc.walk("/iTunes_Control").map_err(io::Error::other)?;
            let doomed: Vec<_> = entries
                .iter()
                .filter(|e| e.size.is_some())
                .filter(|e| {
                    e.path.starts_with("/iTunes_Control/Music/")
                        || e.path.starts_with("/iTunes_Control/Artwork/")
                })
                .map(|e| e.path.clone())
                .collect();
            let bytes: u64 = entries
                .iter()
                .filter(|e| doomed.contains(&e.path))
                .filter_map(|e| e.size)
                .sum();
            writeln!(
                out,
                "This removes {} files ({:.0} MB) of music and artwork, and the music database, from {}.",
                doomed.len(),
                bytes as f64 / 1e6,
                device.udid
            )?;
            if !yes && !ask("Remove it?", false)? {
                writeln!(out, "Nothing changed.")?;
                return Ok(ExitCode::SUCCESS);
            }
            let backed_up = want_backup(always, no_backup)?;
            if backed_up {
                let dir = backup(&device, &mut afc, out)?;
                writeln!(out, "Backed up to {}", dir.display())?;
            }
            let mut failed = 0;
            for path in doomed.iter().map(String::as_str).chain(MUSIC_DATA) {
                match afc.remove(path) {
                    Ok(()) => {}
                    Err(bettertunes_device::Error::Refused(msg)) if msg.contains("not found") => {}
                    Err(e) => {
                        eprintln!("bt: {path}: {e}");
                        failed += 1;
                    }
                }
            }
            if failed > 0 {
                writeln!(out, "{failed} files could not be removed.")?;
                return Ok(ExitCode::FAILURE);
            }
            writeln!(out, "Music removed.")?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The one USB-connected iOS device, or the one matching `wanted`.
fn pick(wanted: Option<&str>) -> Option<Device> {
    let mut devices: Vec<_> = find_devices(|e| eprintln!("bt: {e}"))
        .into_iter()
        .filter(|d| d.connection() != "Network")
        .filter(|d| wanted.is_none_or(|w| d.udid.starts_with(w)))
        .collect();
    match devices.len() {
        1 => devices.pop(),
        0 => {
            eprintln!("bt: no matching iPhone or iPod touch on USB (see `bt devices`)");
            None
        }
        _ => {
            eprintln!("bt: several devices connected; pick one with --device:");
            devices.iter().for_each(|d| eprintln!("    {}", d.udid));
            None
        }
    }
}

/// Asks a yes/no question. Without a terminal, says how to answer with
/// options and returns `None`.
fn ask_opt(prompt: &str, default: bool) -> io::Result<Option<bool>> {
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact()
        .map(Some)
        .map_err(|dialoguer::Error::IO(e)| e)
}

fn ask(prompt: &str, default: bool) -> io::Result<bool> {
    let answer = ask_opt(prompt, default)?;
    if answer.is_none() {
        eprintln!("bt: no terminal to ask in; pass --yes to go ahead");
    }
    Ok(answer == Some(true))
}

/// Whether to back up before erasing: as the options say, or else asked,
/// with a second question before going without. Without a terminal to ask
/// in, backs up.
fn want_backup(always: bool, never: bool) -> io::Result<bool> {
    if always || never {
        return Ok(always);
    }
    let Some(first) = ask_opt("Back up the music to this computer first?", true)? else {
        return Ok(true);
    };
    if first {
        return Ok(true);
    }
    let skip = ask_opt(
        "Erase without a backup? The songs can't be restored.",
        false,
    )?;
    Ok(skip != Some(true))
}

/// Copies `/iTunes_Control` into a new timestamped backup folder, checking
/// every file's size.
fn backup(device: &Device, afc: &mut Afc, out: &mut impl Write) -> io::Result<PathBuf> {
    let base = profile::data_dir().map_err(io::Error::other)?;
    let dir = base.join("backups").join(&device.udid).join(timestamp());
    let entries = afc.walk("/iTunes_Control").map_err(io::Error::other)?;
    let total: u64 = entries.iter().filter_map(|e| e.size).sum();
    writeln!(
        out,
        "Backing up {} files ({:.0} MB)...",
        entries.len(),
        total as f64 / 1e6
    )?;
    for e in &entries {
        let local = dir.join(e.path.trim_start_matches('/'));
        let Some(size) = e.size else {
            std::fs::create_dir_all(&local)?;
            continue;
        };
        let data = afc.read_file(&e.path).map_err(io::Error::other)?;
        if data.len() as u64 != size {
            return Err(io::Error::other(format!(
                "{}: read {} bytes, expected {size}",
                e.path,
                data.len()
            )));
        }
        write_new(&local, &data)?;
    }
    Ok(dir)
}

fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(data)
}

/// UTC "YYYYMMDD-HHMMSS".
fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}
