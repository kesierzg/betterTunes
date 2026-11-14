//! Core of betterTunes: the library data model and readers for the
//! iTunes / Music.app library formats.

pub mod artwork;
mod bytes;
mod chunks;
pub mod clones;
mod error;
pub mod hash58;
pub mod itl;
pub mod itunesdb;
mod model;
pub mod musicdb;
pub mod plist_xml;
pub mod profile;
pub mod pull;
pub mod sync;
pub mod transcode;

pub use error::{Error, Result};
pub use model::{Library, Playlist, Track};

use std::path::{Path, PathBuf};

impl Library {
    /// Opens a library, detecting its format from the file contents.
    ///
    /// Accepts an iTunes `iTunes Library.itl` or its folder, a Music.app
    /// `Music Library.musiclibrary` bundle or its `Library.musicdb`, or an
    /// XML library export (`iTunes Library.xml`).
    pub fn open(path: &Path) -> Result<Library> {
        let file = if path.is_dir() {
            ["Library.musicdb", "iTunes Library.itl"]
                .iter()
                .map(|name| path.join(name))
                .find(|p| p.exists())
                .ok_or_else(|| Error::format("folder contains no library"))?
        } else {
            path.to_path_buf()
        };
        let data = std::fs::read(&file)?;
        match data.get(..4) {
            Some(b"hfma") => musicdb::parse(&data),
            Some(b"hdfm") => itl::parse(&data),
            _ => plist_xml::parse(&data),
        }
    }

    /// Default library locations for the current user, most likely first.
    pub fn default_paths() -> Vec<PathBuf> {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return Vec::new();
        };
        let music = home.join("Music");
        vec![
            music.join("iTunes/iTunes Library.itl"),
            music.join("Music/Music Library.musiclibrary"),
            music.join("iTunes/iTunes Library.xml"),
            music.join("iTunes/iTunes Music Library.xml"),
        ]
    }
}
