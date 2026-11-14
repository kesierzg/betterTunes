//! Per-device sync settings, kept as plists in betterTunes' data folder
//! (`~/Library/Application Support/betterTunes/devices` on macOS,
//! `$XDG_DATA_HOME/bettertunes/devices` elsewhere).

use crate::sync::{Conversion, Selection};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Profile {
    /// UDID for iPhones and iPod touches, FireWire GUID for classic iPods.
    pub id: String,
    /// Device name and model when last seen, for showing and matching.
    pub name: String,
    pub model: String,
    pub storage: Option<Storage>,
    pub selection: Selection,
    pub conversion: Conversion,
}

/// Storage figures from the last time the device was connected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Storage {
    pub total: u64,
    pub free: u64,
    /// Taken by synced music now; freed when the device is re-synced.
    pub music: u64,
}

impl Storage {
    /// Room for music after a sync, keeping a little free as iTunes does.
    pub fn room_for_music(&self) -> u64 {
        let reserve = (self.total / 100).max(200 << 20);
        (self.free + self.music).saturating_sub(reserve)
    }
}

impl Profile {
    pub fn load(id: &str) -> Result<Option<Profile>> {
        match std::fs::read(path(id)?) {
            Ok(bytes) => plist::from_bytes(&bytes)
                .map(Some)
                .map_err(|e| Error::format(format!("profile {id}: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = path(&self.id)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("tmp");
        plist::to_file_xml(&tmp, self).map_err(|e| Error::format(e.to_string()))?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// All saved profiles, by name.
    pub fn all() -> Result<Vec<Profile>> {
        let dir = devices_dir()?;
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut all = Vec::new();
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "plist") {
                if let Some(id) = p.file_stem().and_then(|s| s.to_str()) {
                    all.extend(Profile::load(id)?);
                }
            }
        }
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(all)
    }
}

/// Settings that apply to every device.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Settings {
    /// The library to use instead of this computer's own.
    pub library: Option<PathBuf>,
    /// How the library's song paths map to paths here.
    pub remap: Vec<PathMap>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathMap {
    pub from: PathBuf,
    pub to: PathBuf,
}

impl Settings {
    fn path() -> Result<PathBuf> {
        Ok(data_dir()?.join("settings.plist"))
    }

    pub fn load() -> Result<Settings> {
        match std::fs::read(Settings::path()?) {
            Ok(bytes) => {
                plist::from_bytes(&bytes).map_err(|e| Error::format(format!("settings: {e}")))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Settings::path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("tmp");
        plist::to_file_xml(&tmp, self).map_err(|e| Error::format(e.to_string()))?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

/// For a library on another Mac's disk mounted here, such as
/// `/Volumes/macOS/Users/me/Music/iTunes`, maps that Mac's home folder
/// (`/Users/me`) to where it's mounted (`/Volumes/macOS/Users/me`).
pub fn guess_remap(library: &Path) -> Option<PathMap> {
    let parts: Vec<Component> = library.components().collect();
    let users = parts
        .iter()
        .skip(2)
        .position(|c| c.as_os_str() == "Users")?
        + 2;
    let user = parts.get(users + 1)?;
    let to: PathBuf = parts[..users + 2].iter().collect();
    let from = Path::new("/Users").join(user);
    Some(PathMap { from, to })
}

fn path(id: &str) -> Result<PathBuf> {
    if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
        return Err(Error::format(format!("bad device id {id:?}")));
    }
    Ok(devices_dir()?.join(format!("{id}.plist")))
}

fn devices_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("devices"))
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::format("HOME is not set"))
}

/// betterTunes' data folder.
pub fn data_dir() -> Result<PathBuf> {
    let home = home()?;
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/betterTunes")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("bettertunes")
    })
}

/// Where converted files are kept between syncs.
pub fn cache_dir() -> Result<PathBuf> {
    let home = home()?;
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/Caches/betterTunes")
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
            .join("bettertunes")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::PlaylistRef;

    #[test]
    fn round_trips_as_plist() {
        let profile = Profile {
            id: "abc".into(),
            name: "iPod".into(),
            storage: Some(Storage {
                total: 16 << 30,
                free: 15 << 30,
                music: 1 << 29,
            }),
            selection: Selection {
                playlists: vec![PlaylistRef {
                    persistent_id: Some("1111222233334444".into()),
                    name: "Favourites".into(),
                }],
                genres: vec!["Pop".into()],
                ..Selection::default()
            },
            conversion: Conversion::Aac { kbps: 256 },
            ..Profile::default()
        };
        let mut xml = Vec::new();
        plist::to_writer_xml(&mut xml, &profile).unwrap();
        let back: Profile = plist::from_bytes(&xml).unwrap();
        assert_eq!(back, profile);
        assert!(path("../x").is_err());
    }

    #[test]
    fn guesses_where_another_macs_home_is_mounted() {
        let map = guess_remap(Path::new(
            "/Volumes/macOS/Users/me/Music/iTunes/iTunes Library.itl",
        ))
        .unwrap();
        assert_eq!(map.from, Path::new("/Users/me"));
        assert_eq!(map.to, Path::new("/Volumes/macOS/Users/me"));
        let linux = guess_remap(Path::new("/media/me/macOS/Users/me/Music/iTunes")).unwrap();
        assert_eq!(linux.to, Path::new("/media/me/macOS/Users/me"));
        // This computer's own library needs no mapping.
        assert_eq!(guess_remap(Path::new("/Users/me/Music/iTunes")), None);
    }
}
