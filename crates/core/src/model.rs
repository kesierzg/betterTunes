use std::path::PathBuf;

#[derive(Debug, Default, Clone)]
pub struct Library {
    pub tracks: Vec<Track>,
    pub playlists: Vec<Playlist>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Track {
    /// 64-bit ID iTunes/Music uses to identify the track across syncs.
    pub persistent_id: u64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    /// Human-readable file kind, e.g. "MPEG audio file".
    pub kind: Option<String>,
    pub track_number: Option<u16>,
    pub track_count: Option<u16>,
    pub disc_number: Option<u16>,
    pub disc_count: Option<u16>,
    pub year: Option<u32>,
    pub duration_ms: Option<u32>,
    pub size: Option<u64>,
    /// In kbit/s.
    pub bit_rate: Option<u32>,
    pub sample_rate: Option<u32>,
    /// Local file; `None` for streaming/cloud-only tracks.
    pub location: Option<PathBuf>,
    pub play_count: u32,
    pub skip_count: u32,
    /// The user's rating, 1-100 (20 per star).
    pub rating: Option<u8>,
    /// Last played, in seconds since 1904 local time, as iTunes keeps it.
    pub last_played: Option<u32>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Playlist {
    /// 64-bit ID iTunes uses to identify the playlist; `None` where the
    /// format's playlist ID isn't known yet (Library.musicdb).
    pub persistent_id: Option<u64>,
    pub name: String,
    /// The hidden playlist holding the whole library.
    pub is_master: bool,
    pub is_smart: bool,
    /// iTunes' built-in playlists (Music, Films, Podcasts, Genius...) have a
    /// non-zero kind; the user's own playlists have 0.
    pub distinguished_kind: u8,
    /// Persistent IDs of the tracks, in playlist order.
    pub track_ids: Vec<u64>,
}

impl Playlist {
    /// A playlist the user made, as opposed to the library or a built-in one.
    pub fn is_user(&self) -> bool {
        !self.is_master && self.distinguished_kind == 0
    }
}

impl Library {
    pub fn track(&self, persistent_id: u64) -> Option<&Track> {
        self.tracks
            .iter()
            .find(|t| t.persistent_id == persistent_id)
    }

    /// The user's playlists matching `query`: its persistent ID in hex, its
    /// exact name, or else a case-insensitive part of the name, for names
    /// that are hard to type. Several can share a name.
    pub fn find_playlists(&self, query: &str) -> Vec<&Playlist> {
        let user = || self.playlists.iter().filter(|p| p.is_user());
        let id = (query.len() == 16)
            .then(|| u64::from_str_radix(query, 16).ok())
            .flatten();
        let by_id: Vec<_> = user()
            .filter(|p| id.is_some() && p.persistent_id == id)
            .collect();
        if !by_id.is_empty() {
            return by_id;
        }
        let exact: Vec<_> = user().filter(|p| p.name == query).collect();
        if !exact.is_empty() || query.is_empty() {
            return exact;
        }
        let query = query.to_lowercase();
        user()
            .filter(|p| p.name.to_lowercase().contains(&query))
            .collect()
    }

    /// Total length of a playlist's tracks.
    pub fn duration_ms(&self, playlist: &Playlist) -> u64 {
        let by_id: std::collections::HashMap<u64, &Track> =
            self.tracks.iter().map(|t| (t.persistent_id, t)).collect();
        playlist
            .track_ids
            .iter()
            .filter_map(|id| by_id.get(id)?.duration_ms)
            .map(u64::from)
            .sum()
    }
}

/// Converts a `file://` URL as stored by iTunes/Music into a path.
pub(crate) fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // iTunes on Windows writes file://localhost/C:/...
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    Some(PathBuf::from(percent_decode(rest)))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_urls() {
        assert_eq!(
            file_url_to_path("file:///Users/me/Music/Yellow%20Lake/1-05%20Song.m4a"),
            Some(PathBuf::from("/Users/me/Music/Yellow Lake/1-05 Song.m4a"))
        );
        assert_eq!(
            file_url_to_path("file:///a/%C5%BB%C3%93%C5%81TY%20%C5%9AWIT%zz.m4a"),
            Some(PathBuf::from("/a/ŻÓŁTY ŚWIT%zz.m4a"))
        );
        assert_eq!(file_url_to_path("http://example.com/x.mp3"), None);
    }
}
