//! Reader for the XML library export written by iTunes and Music.app
//! (`iTunes Library.xml`, File > Library > Export Library).

use crate::model::file_url_to_path;
use crate::{Error, Library, Playlist, Result, Track};
use plist::{Dictionary, Value};
use std::collections::HashMap;

pub fn parse(data: &[u8]) -> Result<Library> {
    let root = Value::from_reader(std::io::Cursor::new(data))
        .map_err(|e| Error::format(format!("not a library export: {e}")))?;
    let root = root
        .as_dictionary()
        .ok_or_else(|| Error::format("library export root is not a dictionary"))?;
    let tracks_dict = root
        .get("Tracks")
        .and_then(Value::as_dictionary)
        .ok_or_else(|| Error::format("library export has no Tracks"))?;

    let mut lib = Library::default();
    // Playlists reference tracks by the export-local "Track ID".
    let mut by_track_id = HashMap::new();
    for t in tracks_dict.values().filter_map(Value::as_dictionary) {
        let track = parse_track(t);
        if let Some(id) = uint(t, "Track ID") {
            by_track_id.insert(id, track.persistent_id);
        }
        lib.tracks.push(track);
    }

    for p in root
        .get("Playlists")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_dictionary)
    {
        let items = p.get("Playlist Items").and_then(Value::as_array);
        lib.playlists.push(Playlist {
            persistent_id: string(p, "Playlist Persistent ID")
                .and_then(|h| u64::from_str_radix(&h, 16).ok()),
            name: string(p, "Name").unwrap_or_default(),
            is_master: p.get("Master").and_then(Value::as_boolean) == Some(true),
            is_smart: p.contains_key("Smart Info"),
            distinguished_kind: uint(p, "Distinguished Kind")
                .and_then(|k| u8::try_from(k).ok())
                .unwrap_or(0),
            track_ids: items
                .into_iter()
                .flatten()
                .filter_map(|i| uint(i.as_dictionary()?, "Track ID"))
                .filter_map(|id| by_track_id.get(&id).copied())
                .collect(),
        });
    }
    Ok(lib)
}

fn string(d: &Dictionary, key: &str) -> Option<String> {
    d.get(key)?.as_string().map(str::to_owned)
}

fn uint(d: &Dictionary, key: &str) -> Option<u64> {
    d.get(key)?.as_unsigned_integer()
}

fn parse_track(d: &Dictionary) -> Track {
    let persistent_id = string(d, "Persistent ID")
        .and_then(|h| u64::from_str_radix(&h, 16).ok())
        .or_else(|| uint(d, "Track ID"))
        .unwrap_or_default();
    Track {
        persistent_id,
        title: string(d, "Name"),
        artist: string(d, "Artist"),
        album: string(d, "Album"),
        album_artist: string(d, "Album Artist"),
        genre: string(d, "Genre"),
        composer: string(d, "Composer"),
        kind: string(d, "Kind"),
        track_number: uint(d, "Track Number").and_then(|n| n.try_into().ok()),
        track_count: uint(d, "Track Count").and_then(|n| n.try_into().ok()),
        disc_number: uint(d, "Disc Number").and_then(|n| n.try_into().ok()),
        disc_count: uint(d, "Disc Count").and_then(|n| n.try_into().ok()),
        year: uint(d, "Year").and_then(|n| n.try_into().ok()),
        duration_ms: uint(d, "Total Time").and_then(|n| n.try_into().ok()),
        size: uint(d, "Size"),
        bit_rate: uint(d, "Bit Rate").and_then(|n| n.try_into().ok()),
        sample_rate: uint(d, "Sample Rate").and_then(|n| n.try_into().ok()),
        play_count: uint(d, "Play Count")
            .and_then(|n| n.try_into().ok())
            .unwrap_or(0),
        skip_count: uint(d, "Skip Count")
            .and_then(|n| n.try_into().ok())
            .unwrap_or(0),
        // Ratings iTunes derives from the album's aren't the user's.
        rating: uint(d, "Rating")
            .filter(|_| d.get("Rating Computed").and_then(Value::as_boolean) != Some(true))
            .and_then(|n| u8::try_from(n).ok())
            .filter(|&r| r > 0),
        last_played: uint(d, "Play Date").and_then(|n| u32::try_from(n).ok()),
        location: string(d, "Location").and_then(|u| file_url_to_path(&u)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Major Version</key><integer>1</integer>
	<key>Tracks</key>
	<dict>
		<key>101</key>
		<dict>
			<key>Track ID</key><integer>101</integer>
			<key>Name</key><string>Test Song 2002</string>
			<key>Artist</key><string>Yellow Lake</string>
			<key>Kind</key><string>MPEG audio file</string>
			<key>Size</key><integer>4825088</integer>
			<key>Total Time</key><integer>201064</integer>
			<key>Track Number</key><integer>3</integer>
			<key>Year</key><integer>2013</integer>
			<key>Bit Rate</key><integer>192</integer>
			<key>Sample Rate</key><integer>44100</integer>
			<key>Persistent ID</key><string>0A0B0C0D0E0F1011</string>
			<key>Location</key><string>file://localhost/Users/me/Music/Test%20Song.mp3</string>
		</dict>
	</dict>
	<key>Playlists</key>
	<array>
		<dict>
			<key>Name</key><string>Library</string>
			<key>Master</key><true/>
			<key>Playlist Persistent ID</key><string>0123456789ABCDEF</string>
			<key>Playlist Items</key>
			<array><dict><key>Track ID</key><integer>101</integer></dict></array>
		</dict>
		<dict>
			<key>Name</key><string>Recently Added</string>
			<key>Smart Info</key><data>AQEAAwAAAAIAAAAZ</data>
		</dict>
	</array>
</dict>
</plist>"#;

    #[test]
    fn parses_itunes_export() {
        let lib = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(lib.tracks.len(), 1);
        let t = &lib.tracks[0];
        assert_eq!(t.persistent_id, 0x0A0B_0C0D_0E0F_1011);
        assert_eq!(t.title.as_deref(), Some("Test Song 2002"));
        assert_eq!(t.duration_ms, Some(201_064));
        assert_eq!(t.track_number, Some(3));
        assert_eq!(
            t.location,
            Some(PathBuf::from("/Users/me/Music/Test Song.mp3"))
        );

        assert_eq!(lib.playlists.len(), 2);
        assert!(lib.playlists[0].is_master);
        assert_eq!(lib.playlists[0].persistent_id, Some(0x0123_4567_89AB_CDEF));
        assert_eq!(lib.playlists[1].persistent_id, None);
        assert_eq!(lib.playlists[0].track_ids, vec![0x0A0B_0C0D_0E0F_1011]);
        assert!(lib.playlists[1].is_smart);
        assert!(lib.playlists[1].track_ids.is_empty());
    }

    #[test]
    fn rejects_non_library_plists() {
        assert!(parse(b"<plist version=\"1.0\"><array/></plist>").is_err());
        assert!(parse(b"garbage").is_err());
    }
}
